use smithay::desktop::{
    PopupKind, Window, WindowSurfaceType, find_popup_root_surface, layer_map_for_output,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::shell::xdg::{PopupSurface, PositionerState};

use super::Beewm;

#[derive(Debug, Clone, Copy)]
struct PopupConstraintSpace {
    parent_geometry: Rectangle<i32, Logical>,
    output_geometry: Rectangle<i32, Logical>,
}

pub fn is_fixed_size(size: Size<i32, smithay::utils::Logical>) -> bool {
    size.w > 0 && size.h > 0
}

/// True when a toplevel's declared `max_size` is a *plausible dialog cap* —
/// both axes bounded and small enough to be a dialog, rather than a "no real
/// max" sentinel (display dimensions, 32767) that ordinary resizable app
/// windows routinely publish.
///
/// The earlier rule ("any `max > 0` on either axis is a dialog") floated normal
/// parent app windows that merely advertise a large cap; that is the root cause
/// of the keyring prompt's parent appearing floating *and* being stacked above
/// the prompt (`raise_floating_windows` raises the most-recently-inserted
/// floating window — the parent — on top). Keeping the parent tiled fixes both.
pub fn is_dialog_size_cap(max_size: Size<i32, Logical>) -> bool {
    max_size.w > 0 && max_size.h > 0 && max_size.w <= 1280 && max_size.h <= 1024
}

/// Choose the top-left placement for a floating dialog of `win_size`.
///
/// When `parent` is given the dialog is centred over the parent rectangle
/// (matching how stacking WMs place transient/modal dialogs over the window
/// they belong to); otherwise it is centred within `usable`. The result is
/// always clamped so the dialog stays fully inside `usable` whenever it fits,
/// and pinned to the usable origin when the dialog is larger than the screen.
pub fn centered_dialog_position(
    usable: Rectangle<i32, Logical>,
    parent: Option<Rectangle<i32, Logical>>,
    win_size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let anchor = parent.unwrap_or(usable);
    let x = anchor.loc.x + (anchor.size.w - win_size.w) / 2;
    let y = anchor.loc.y + (anchor.size.h - win_size.h) / 2;
    let max_x = usable.loc.x + (usable.size.w - win_size.w).max(0);
    let max_y = usable.loc.y + (usable.size.h - win_size.h).max(0);
    Point::from((x.clamp(usable.loc.x, max_x), y.clamp(usable.loc.y, max_y)))
}

/// Last-resort allowlist for authentication/keyring prompters that float as
/// dialogs but may not carry a parent, modal flag, or size cap on the wire
/// (e.g. GNOME Keyring's `gcr-prompter` and polkit agents run as their own
/// D-Bus-activated clients, so cross-client `set_parent` is impossible).
///
/// This is intentionally narrow: protocol metadata (parent/modal/size hints)
/// is the primary signal — this only catches prompters when that metadata is
/// absent, and never overrides a window that announces itself as normal.
pub(crate) fn is_known_dialog_app_id(app_id: &str) -> bool {
    const KNOWN: &[&str] = &[
        "gcr-prompter",
        "org.gnome.keyring.Prompter",
        "gnome-keyring-prompter",
        "polkit-gnome-authentication-agent-1",
        "org.freedesktop.PolicyKit1.Authority",
        "polkit-kde-authentication-agent-1",
    ];
    KNOWN.iter().any(|known| app_id.eq_ignore_ascii_case(known))
}

pub fn popup_constraint_target(
    parent_geometry: Rectangle<i32, Logical>,
    output_geometry: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    Rectangle::new(
        output_geometry.loc - parent_geometry.loc,
        output_geometry.size,
    )
}

pub fn constrain_popup_geometry(
    positioner: PositionerState,
    parent_geometry: Rectangle<i32, Logical>,
    output_geometry: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    positioner.get_unconstrained_geometry(popup_constraint_target(parent_geometry, output_geometry))
}

/// Per-window floating classification for an xdg toplevel.
///
/// Classification is **per window**: a window floats because of its *own*
/// properties (parent set on itself, its own modal flag, its own size hints).
/// It is never derived from a child or parent — a normal app that merely *has*
/// a dialog child stays tiled.
#[derive(Debug, Clone)]
pub(crate) struct ToplevelFloatClass {
    pub should_float: bool,
    /// Stable, human-readable reason for the decision (for diagnostics).
    pub reason: &'static str,
    pub has_parent: bool,
    pub is_modal: bool,
    pub known_dialog: bool,
    pub app_id: Option<String>,
    pub title: Option<String>,
}

pub(crate) fn classify_toplevel_floating(window: &Window) -> ToplevelFloatClass {
    let Some(toplevel) = window.toplevel() else {
        return ToplevelFloatClass {
            should_float: false,
            reason: "not-a-toplevel",
            has_parent: false,
            is_modal: false,
            known_dialog: false,
            app_id: None,
            title: None,
        };
    };

    smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        let (has_parent, is_modal, title, app_id) = states
            .data_map
            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
            .map(|role| {
                let role = role.lock().unwrap();
                (
                    role.parent.is_some(),
                    role.modal,
                    role.title.clone(),
                    role.app_id.clone(),
                )
            })
            .unwrap_or((false, false, None, None));
        let mut cached = states
            .cached_state
            .get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
        let current = *cached.current();
        // Strictly fixed-size (min == max, both positive) is the textbook
        // non-resizable-dialog signal.
        let is_fixed = is_fixed_size(current.min_size) && current.min_size == current.max_size;
        // A *plausible* dialog cap (see `is_dialog_size_cap`): both axes bounded
        // and within dialog-plausible bounds, so a parent app that merely
        // advertises a large/sentinel max_size stays tiled.
        let has_size_cap = is_dialog_size_cap(current.max_size);
        // Protocol metadata is the primary signal; the app_id allowlist is only
        // consulted as a fallback for prompters that carry none of it.
        let known_dialog = app_id
            .as_deref()
            .map(is_known_dialog_app_id)
            .unwrap_or(false);

        // Order matters only for the diagnostic reason string; the OR below is
        // what actually decides. Strongest/most-specific signals first.
        let (should_float, reason) = if has_parent {
            (true, "has-parent")
        } else if is_modal {
            (true, "modal")
        } else if known_dialog {
            (true, "known-dialog-app-id")
        } else if is_fixed {
            (true, "fixed-size")
        } else if has_size_cap {
            (true, "bounded-size-cap")
        } else {
            (false, "normal")
        };

        tracing::debug!(
            target = "beewm::floating",
            ?title,
            ?app_id,
            has_parent,
            is_modal,
            min_size = ?current.min_size,
            max_size = ?current.max_size,
            has_size_cap,
            is_fixed,
            known_dialog,
            should_float,
            reason,
            "classify_toplevel_floating decision",
        );

        ToplevelFloatClass {
            should_float,
            reason,
            has_parent,
            is_modal,
            known_dialog,
            app_id,
            title,
        }
    })
}

pub(crate) fn should_map_toplevel_floating(window: &Window) -> bool {
    classify_toplevel_floating(window).should_float
}

impl Beewm {
    fn output_geometry_for_rectangle(
        &self,
        rectangle: Rectangle<i32, Logical>,
    ) -> Option<Rectangle<i32, Logical>> {
        let center = Point::from((
            rectangle.loc.x + rectangle.size.w / 2,
            rectangle.loc.y + rectangle.size.h / 2,
        ));

        self.space
            .output_under(center.to_f64())
            .find_map(|output| self.space.output_geometry(output))
            .or_else(|| {
                self.space.outputs().find_map(|output| {
                    let output_geometry = self.space.output_geometry(output)?;
                    output_geometry
                        .intersection(rectangle)
                        .map(|_| output_geometry)
                })
            })
            .or_else(|| {
                self.focused_output()
                    .and_then(|output| self.space.output_geometry(&output))
            })
    }

    fn popup_constraint_space_for_popup(&self, popup: &PopupKind) -> Option<PopupConstraintSpace> {
        let PopupKind::Xdg(parent_popup) = popup else {
            return None;
        };

        let parent_surface = parent_popup.get_parent_surface()?;
        let parent_space = self.popup_constraint_space_for_surface(&parent_surface)?;
        let geometry = popup.geometry();

        Some(PopupConstraintSpace {
            parent_geometry: Rectangle::new(
                parent_space.parent_geometry.loc + geometry.loc,
                geometry.size,
            ),
            output_geometry: parent_space.output_geometry,
        })
    }

    fn popup_constraint_space_for_layer_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<PopupConstraintSpace> {
        self.space.outputs().find_map(|output| {
            let (layer, layer_geometry) = {
                let layer_map = layer_map_for_output(output);
                let layer = layer_map
                    .layer_for_surface(
                        surface,
                        WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
                    )
                    .cloned()?;
                let layer_geometry = layer_map.layer_geometry(&layer)?;
                Some((layer, layer_geometry))
            }?;

            let output_geometry = self.space.output_geometry(output)?;
            let surface_origin = layer_geometry.loc - layer.bbox().loc;
            let current_size = layer
                .layer_surface()
                .current_state()
                .size
                .filter(|size| size.w > 0 && size.h > 0);
            let cached_size = layer.cached_state().size;
            let parent_size = current_size
                .or_else(|| (cached_size.w > 0 && cached_size.h > 0).then_some(cached_size))
                .unwrap_or(layer_geometry.size);

            Some(PopupConstraintSpace {
                parent_geometry: Rectangle::new(surface_origin, parent_size),
                output_geometry,
            })
        })
    }

    fn popup_constraint_space_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<PopupConstraintSpace> {
        if let Some(popup) = self.popup_manager.find_popup(surface) {
            return self.popup_constraint_space_for_popup(&popup);
        }

        if let Some(window) = self.mapped_window_for_surface(surface) {
            let parent_geometry = self.space.element_geometry(&window)?;
            let output_geometry = self.output_geometry_for_rectangle(parent_geometry)?;
            return Some(PopupConstraintSpace {
                parent_geometry,
                output_geometry,
            });
        }

        self.popup_constraint_space_for_layer_surface(surface)
    }

    pub(crate) fn configure_xdg_popup(&self, surface: &PopupSurface, positioner: PositionerState) {
        let parent_surface = surface.get_parent_surface();
        let root_surface = find_popup_root_surface(&PopupKind::Xdg(surface.clone())).ok();
        let constraint_space = parent_surface
            .as_ref()
            .and_then(|parent| self.popup_constraint_space_for_surface(parent));
        let geometry = constraint_space
            .map(|space| {
                constrain_popup_geometry(positioner, space.parent_geometry, space.output_geometry)
            })
            .unwrap_or_else(|| {
                tracing::warn!(
                    popup_surface = ?surface.wl_surface(),
                    parent_surface = ?parent_surface.as_ref(),
                    root_surface = ?root_surface.as_ref(),
                    "Failed to resolve popup constraint space; falling back to raw positioner geometry",
                );
                positioner.get_geometry()
            });

        if let Some(space) = constraint_space {
            tracing::debug!(
                popup_surface = ?surface.wl_surface(),
                parent_surface = ?parent_surface.as_ref(),
                root_surface = ?root_surface.as_ref(),
                parent_geometry = ?space.parent_geometry,
                output_geometry = ?space.output_geometry,
                popup_geometry = ?geometry,
                reactive = positioner.reactive,
                "Configured xdg_popup geometry",
            );
        }

        surface.with_pending_state(|state| {
            state.positioner = positioner;
            state.geometry = geometry;
        });
    }
}
