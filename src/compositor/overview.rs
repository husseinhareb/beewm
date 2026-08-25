//! Hold-Super window overview ("task view").
//!
//! Holding Super for [`HOLD_DELAY`] without pressing anything else brings up a
//! grid of live thumbnails of every window on every workspace — 10 windows on a
//! 16:9 screen give 5 columns × 2 rows. Tab/arrow keys or the pointer move the
//! selection; releasing Super activates the selected window and the grid
//! disappears. Pressing any other key (or a mouse button) while Super is still
//! down cancels the pending grid, so ordinary `mod+…` binds and `mod+drag` are
//! completely untouched and never see it flash.
//!
//! The thumbnails are the *live* client surfaces scaled into their cell with
//! `constrain_space_element` — the same transform stack the window animations
//! use — so there is no offscreen capture, texture copy or extra render pass.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::utils::{ConstrainAlign, ConstrainScaleBehavior};
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Color32F, ImportAll, Renderer, Texture};
use smithay::desktop::Window;
use smithay::desktop::space::{ConstrainBehavior, ConstrainReference, constrain_space_element};
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::output::Output;
use smithay::utils::{IsAlive, Logical, Point, Rectangle, Scale};
use smithay::wayland::seat::WaylandFocus;

use crate::compositor::render::WindowElement;
use crate::compositor::state::Beewm;

/// How long Super must be held down, with nothing else pressed, before the grid
/// appears. Long enough that `mod+<key>` binds and `mod+drag` never flash it.
pub const HOLD_DELAY: Duration = Duration::from_millis(180);

/// Empty space kept between the grid and the edges of the output.
const MARGIN: i32 = 48;
/// Space between grid cells.
const GAP: i32 = 16;
/// Padding between a cell's edge and its thumbnail — i.e. the visible width of
/// the selection frame.
const CELL_PADDING: i32 = 6;

/// Dimmed backdrop drawn over the desktop. Slightly translucent so the session
/// stays recognisable underneath.
const BACKDROP: Color32F = Color32F::new(0.04, 0.04, 0.06, 0.88);
/// Card drawn behind every thumbnail, so letterboxed and not-yet-drawn cells
/// still read as a tile.
const CARD: Color32F = Color32F::new(0.16, 0.16, 0.19, 1.0);

/// Which way a navigation key moves the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverviewNav {
    Next,
    Prev,
    Left,
    Right,
    Up,
    Down,
}

/// Number of columns to lay `count` thumbnails out in on a screen whose aspect
/// ratio (width / height) is `aspect`.
///
/// `ceil(sqrt(count * aspect))` keeps each cell close to the screen's own
/// proportions, which is what makes 10 windows on a 16:9 output come out as
/// 5 columns × 2 rows rather than a squarer 4 × 3.
pub fn grid_columns(count: usize, aspect: f64) -> usize {
    if count == 0 {
        return 0;
    }
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    };
    (((count as f64) * aspect).sqrt().ceil() as usize).clamp(1, count)
}

/// Lay `count` cells out over `area`, `gap` apart, with a partial last row
/// centered. The returned rectangles are in the same coordinate space as
/// `area` and index-aligned with the window list.
pub fn cell_rects(
    count: usize,
    area: Rectangle<i32, Logical>,
    gap: i32,
) -> Vec<Rectangle<i32, Logical>> {
    if count == 0 || area.size.w <= 0 || area.size.h <= 0 {
        return Vec::new();
    }

    let cols = grid_columns(count, area.size.w as f64 / area.size.h as f64);
    let rows = count.div_ceil(cols);
    // `max(1)` keeps a degenerate cell (very many windows on a small output)
    // renderable instead of inverted.
    let cell_w = ((area.size.w - gap * (cols as i32 - 1)) / cols as i32).max(1);
    let cell_h = ((area.size.h - gap * (rows as i32 - 1)) / rows as i32).max(1);

    (0..count)
        .map(|index| {
            let row = index / cols;
            let col = index % cols;
            let in_row = (count - row * cols).min(cols) as i32;
            let row_width = in_row * cell_w + (in_row - 1) * gap;
            let x = area.loc.x + (area.size.w - row_width) / 2 + col as i32 * (cell_w + gap);
            let y = area.loc.y + row as i32 * (cell_h + gap);
            Rectangle::new((x, y).into(), (cell_w, cell_h).into())
        })
        .collect()
}

/// Where `nav` moves a selection of `selected` in a `cols`-wide grid of `count`
/// cells. Tab/Shift+Tab wrap around the whole grid; the arrow keys stay put at
/// the edges instead of jumping to the other side.
pub fn nav_target(selected: usize, count: usize, cols: usize, nav: OverviewNav) -> usize {
    if count == 0 {
        return 0;
    }
    let cols = cols.max(1);
    let selected = selected.min(count - 1);
    match nav {
        OverviewNav::Next => (selected + 1) % count,
        OverviewNav::Prev => (selected + count - 1) % count,
        OverviewNav::Left => {
            if selected.is_multiple_of(cols) {
                selected
            } else {
                selected - 1
            }
        }
        OverviewNav::Right => {
            let next = selected + 1;
            if next.is_multiple_of(cols) || next >= count {
                selected
            } else {
                next
            }
        }
        OverviewNav::Up => selected.checked_sub(cols).unwrap_or(selected),
        OverviewNav::Down => {
            let next = selected + cols;
            if next < count { next } else { selected }
        }
    }
}

fn nav_for_keysym(keysym: Keysym, shift: bool) -> Option<OverviewNav> {
    let nav = match keysym {
        Keysym::Tab if shift => OverviewNav::Prev,
        Keysym::Tab => OverviewNav::Next,
        Keysym::ISO_Left_Tab => OverviewNav::Prev,
        Keysym::Left => OverviewNav::Left,
        Keysym::Right => OverviewNav::Right,
        Keysym::Up => OverviewNav::Up,
        Keysym::Down => OverviewNav::Down,
        _ => return None,
    };
    Some(nav)
}

/// Modifier keys that are never a selection or a dismissal on their own.
fn is_modifier_keysym(keysym: Keysym) -> bool {
    matches!(
        keysym,
        Keysym::Shift_L
            | Keysym::Shift_R
            | Keysym::Control_L
            | Keysym::Control_R
            | Keysym::Alt_L
            | Keysym::Alt_R
            | Keysym::Meta_L
            | Keysym::Meta_R
            | Keysym::Hyper_L
            | Keysym::Hyper_R
            | Keysym::Caps_Lock
            | Keysym::Shift_Lock
            | Keysym::Num_Lock
            | Keysym::ISO_Level3_Shift
            | Keysym::ISO_Level5_Shift
    )
}

/// One thumbnail: the window and the workspace it lives on, so activating it
/// can switch workspaces first.
pub(crate) struct OverviewItem {
    pub window: Window,
    pub workspace: usize,
}

/// The open grid. Built once when it opens and thrown away when it closes, so
/// the cell geometry and the render-element IDs stay stable while it is up.
pub(crate) struct Overview {
    pub items: Vec<OverviewItem>,
    /// Cell rectangles in output-local logical coordinates, index-aligned with
    /// `items`.
    pub cells: Vec<Rectangle<i32, Logical>>,
    pub cols: usize,
    pub selected: usize,
    /// The output the grid is drawn on: the focused one when it opened.
    pub output: Output,
    backdrop_id: Id,
    selection_id: Id,
    cell_ids: Vec<Id>,
}

impl Beewm {
    /// Feed one key event to the overview state machine, ahead of keybind
    /// matching. Returns `true` when the event was consumed and must reach
    /// neither a keybind nor the focused client.
    ///
    /// Super itself is tracked through `modifiers.logo` rather than the
    /// Super_L/Super_R keysyms, so either Super key (and either order of
    /// pressing both) behaves the same.
    pub(crate) fn overview_handle_key(
        &mut self,
        modifiers: &ModifiersState,
        keysym: Keysym,
        pressed: bool,
        now: Instant,
    ) -> bool {
        let logo_before = std::mem::replace(&mut self.logo_held, modifiers.logo);

        if !pressed {
            // Never intercept a release: the client must always see the key go
            // up, or it is left with a stuck modifier.
            if logo_before && !modifiers.logo {
                self.overview_hold = None;
                if self.overview.is_some() {
                    self.close_overview(true);
                }
            }
            return false;
        }

        if modifiers.logo && !logo_before {
            // Super just went down on its own: arm the hold.
            if self.config.overview_enabled && !self.locked && self.active_grab.is_none() {
                self.overview_hold = Some(now);
            }
            return false;
        }

        // A second Super key while one is already down is still just "Super".
        if matches!(keysym, Keysym::Super_L | Keysym::Super_R) {
            return false;
        }

        // Another modifier is either the start of a chord (`mod+shift+…`), in
        // which case the pending grid is cancelled, or Shift for Shift+Tab on a
        // grid that is already up — which must not dismiss it.
        if is_modifier_keysym(keysym) {
            self.overview_hold = None;
            return false;
        }

        // Any other key means the user is typing a binding, not asking for the
        // grid.
        self.overview_hold = None;
        if self.overview.is_none() {
            return false;
        }

        if let Some(nav) = nav_for_keysym(keysym, modifiers.shift) {
            self.overview_nav(nav);
            return true;
        }
        match keysym {
            Keysym::Escape => {
                self.close_overview(false);
                true
            }
            Keysym::Return | Keysym::KP_Enter | Keysym::space => {
                self.close_overview(true);
                true
            }
            // Anything else dismisses the grid and runs as the keybind it was
            // meant to be.
            _ => {
                self.close_overview(false);
                false
            }
        }
    }

    /// Cancel a pending (not yet visible) grid — used when a pointer button
    /// goes down, so `mod+click` drags never turn into an overview.
    pub(crate) fn cancel_overview_hold(&mut self) {
        self.overview_hold = None;
    }

    /// Open the grid once Super has been held long enough. Called once per main
    /// loop turn from the backends, next to `tick_animations`.
    pub fn tick_overview(&mut self, now: Instant) {
        let Some(since) = self.overview_hold else {
            return;
        };
        if self.overview.is_some() || now.duration_since(since) < HOLD_DELAY {
            return;
        }
        // Consumed either way: an overview that could not open must not be
        // retried on every following turn.
        self.overview_hold = None;
        if self.locked || self.active_grab.is_some() {
            return;
        }
        self.open_overview();
    }

    fn open_overview(&mut self) {
        let Some(output) = self.focused_output() else {
            return;
        };
        let Some(region) = self.space.output_geometry(&output) else {
            return;
        };

        // Every window on every workspace, in workspace order. Sticky windows
        // live on a single workspace but are drawn on all of them, so dedupe by
        // root surface to be safe.
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        for (workspace_idx, workspace) in self.workspaces.iter().enumerate() {
            for window in &workspace.windows {
                if !window.alive() {
                    continue;
                }
                let Some(root) = Self::window_root_surface(window) else {
                    continue;
                };
                if !seen.insert(root) {
                    continue;
                }
                items.push(OverviewItem {
                    window: window.clone(),
                    workspace: workspace_idx,
                });
            }
        }
        if items.is_empty() {
            return;
        }

        let area = Rectangle::new(
            (MARGIN, MARGIN).into(),
            (
                (region.size.w - MARGIN * 2).max(1),
                (region.size.h - MARGIN * 2).max(1),
            )
                .into(),
        );
        let cells = cell_rects(items.len(), area, GAP);
        let cols = grid_columns(items.len(), area.size.w as f64 / area.size.h as f64);

        // Start on the currently focused window so a hold-and-release with no
        // navigation is a no-op rather than a surprise focus change.
        let focused_root = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|target| target.wl_surface().map(|surface| surface.into_owned()));
        let selected = focused_root
            .and_then(|focused| {
                items.iter().position(|item| {
                    Self::window_root_surface(&item.window)
                        .map(|root| root == focused)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(0);

        self.overview = Some(Overview {
            cell_ids: (0..items.len()).map(|_| Id::new()).collect(),
            items,
            cells,
            cols,
            selected,
            output,
            backdrop_id: Id::new(),
            selection_id: Id::new(),
        });
        self.needs_render = true;
    }

    /// Dismiss the grid, focusing the selected window when `activate` is set.
    pub(crate) fn close_overview(&mut self, activate: bool) {
        let Some(overview) = self.overview.take() else {
            return;
        };
        self.needs_render = true;

        if !activate {
            return;
        }
        let Some(item) = overview.items.get(overview.selected) else {
            return;
        };
        if !item.window.alive() {
            return;
        }
        let Some(root) = Self::window_root_surface(&item.window) else {
            return;
        };
        if item.workspace != self.active_workspace() {
            self.switch_workspace(item.workspace);
        }
        if let Some(idx) = self.window_index_for_surface(self.active_workspace(), &root) {
            self.focus_active_workspace_window(idx);
        }
    }

    fn set_overview_selection(&mut self, selected: usize) {
        let Some(overview) = self.overview.as_mut() else {
            return;
        };
        if overview.selected == selected {
            return;
        }
        overview.selected = selected;
        self.needs_render = true;
    }

    pub(crate) fn overview_nav(&mut self, nav: OverviewNav) {
        let Some(overview) = self.overview.as_ref() else {
            return;
        };
        let target = nav_target(overview.selected, overview.items.len(), overview.cols, nav);
        self.set_overview_selection(target);
    }

    /// Hover-select while the grid is up. Returns `true` when the motion was
    /// consumed, so the pointer never reaches a client behind the grid.
    pub(crate) fn overview_pointer_moved(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some(overview) = self.overview.as_ref() else {
            return false;
        };
        let Some(region) = self.space.output_geometry(&overview.output) else {
            return true;
        };
        let local = pos - region.loc.to_f64();
        if let Some(idx) = overview
            .cells
            .iter()
            .position(|cell| cell.to_f64().contains(local))
        {
            self.set_overview_selection(idx);
        }
        true
    }

    /// A pointer button while the grid is up picks the hovered thumbnail.
    /// Returns `true` when the click was consumed.
    pub(crate) fn overview_pointer_pressed(&mut self) -> bool {
        if self.overview.is_none() {
            return false;
        }
        self.close_overview(true);
        true
    }
}

fn solid(
    id: Id,
    rect: Rectangle<i32, Logical>,
    scale: Scale<f64>,
    color: Color32F,
) -> SolidColorRenderElement {
    SolidColorRenderElement::new(
        id,
        rect.to_physical_precise_round::<f64, i32>(scale),
        CommitCounter::default(),
        color,
        Kind::Unspecified,
    )
}

/// Build the overview's render elements for `output`, front-to-back within each
/// returned list: the thumbnails go above the quads, and both go above
/// everything else on screen.
///
/// Returns empty lists when the grid is closed or `output` is not the one it
/// opened on, so the other outputs keep rendering their desktop normally.
pub(crate) fn overview_elements<R>(
    state: &Beewm,
    renderer: &mut R,
    output: &Output,
) -> (Vec<SolidColorRenderElement>, Vec<WindowElement<R>>)
where
    R: Renderer + ImportAll,
    R::TextureId: Texture + Clone + 'static,
{
    let Some(overview) = state.overview.as_ref() else {
        return (Vec::new(), Vec::new());
    };
    if overview.output != *output {
        return (Vec::new(), Vec::new());
    }
    let Some(region) = state.space.output_geometry(output) else {
        return (Vec::new(), Vec::new());
    };
    let scale = Scale::from(output.current_scale().fractional_scale());

    let mut thumbnails = Vec::new();
    for (item, cell) in overview.items.iter().zip(&overview.cells) {
        let inner = Rectangle::new(
            (cell.loc.x + CELL_PADDING, cell.loc.y + CELL_PADDING).into(),
            (
                (cell.size.w - CELL_PADDING * 2).max(1),
                (cell.size.h - CELL_PADDING * 2).max(1),
            )
                .into(),
        );
        // `Fit` + centered keeps every window's own aspect ratio and letterboxes
        // it inside the cell; the card behind it fills the rest.
        thumbnails.extend(constrain_space_element::<R, Window, WindowElement<R>>(
            renderer,
            &item.window,
            inner.loc,
            1.0,
            scale,
            inner,
            ConstrainBehavior {
                reference: ConstrainReference::Geometry,
                behavior: ConstrainScaleBehavior::Fit,
                align: ConstrainAlign::CENTER,
            },
        ));
    }

    let mut quads = Vec::with_capacity(overview.cells.len() + 2);
    // The selection frame is one moving quad rather than a per-cell colour
    // swap: the damage tracker notices a geometry change on the same element
    // ID, but not a colour change at identical geometry.
    if let Some(cell) = overview.cells.get(overview.selected) {
        quads.push(solid(
            overview.selection_id.clone(),
            *cell,
            scale,
            state.border_color_focused,
        ));
    }
    for (id, cell) in overview.cell_ids.iter().zip(&overview.cells) {
        quads.push(solid(id.clone(), *cell, scale, CARD));
    }
    quads.push(solid(
        overview.backdrop_id.clone(),
        Rectangle::from_size(region.size),
        scale,
        BACKDROP,
    ));

    (quads, thumbnails)
}
