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

pub(crate) fn should_map_toplevel_floating(window: &Window) -> bool {
    let Some(toplevel) = window.toplevel() else {
        return false;
    };

    if toplevel.parent().is_some() {
        return true;
    }

    smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        let mut cached = states
            .cached_state
            .get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
        let current = *cached.current();
        is_fixed_size(current.min_size) && current.min_size == current.max_size
    })
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
                self.space
                    .outputs()
                    .next()
                    .and_then(|output| self.space.output_geometry(output))
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
