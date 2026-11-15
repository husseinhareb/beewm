//! Hold-Super window overview ("task view").
//!
//! Holding Super for [`HOLD_DELAY`] without pressing anything else brings up a
//! grid of miniature screens, one per non-empty workspace, each showing live
//! thumbnails of its windows at their real positions. Tab/arrow keys or the
//! pointer move the
//! selection; releasing Super activates the selected window and the grid
//! disappears. Pressing any other key (or a mouse button) while Super is still
//! down cancels the pending grid, so ordinary `mod+…` binds and `mod+drag` are
//! completely untouched and never see it flash.
//!
//! The thumbnails are the *live* client surfaces scaled into their card with
//! `constrain_space_element` — the same transform stack the window animations
//! use — so there is no offscreen capture, texture copy or extra render pass.

use std::borrow::BorrowMut;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::ImportMem;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::utils::{ConstrainAlign, ConstrainScaleBehavior};
use smithay::backend::renderer::element::{Id, Kind};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Color32F, ImportAll, Renderer, Texture};
use smithay::desktop::Window;
use smithay::desktop::space::{ConstrainBehavior, ConstrainReference, constrain_space_element};
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::seat::WaylandFocus;

use crate::compositor::render::WindowElement;
use crate::compositor::state::{Beewm, focused_window_title};
use crate::compositor::{appicon, overview_chrome, text};

/// How long Super must be held down, with nothing else pressed, before the grid
/// appears. Long enough that `mod+<key>` binds and `mod+drag` never flash it.
pub const HOLD_DELAY: Duration = Duration::from_millis(180);

/// Empty space kept between the grid and the edges of the output.
const MARGIN: i32 = 48;
/// Space between workspace panels.
const GAP: i32 = 32;
/// Width of the outline drawn around every cell. It sits *outside* the cell
/// rather than eating into it, so the thumbnail still fills the cell exactly
/// and a full-screen window has no matting at all.
const FRAME: i32 = 3;

/// Dimmed backdrop drawn over the desktop. Deliberately translucent: the point
/// of the overview is to pick a window out of what you were already looking at,
/// so the desktop stays visible underneath rather than being blacked out.
const BACKDROP: Color32F = Color32F::new(0.03, 0.03, 0.05, 0.55);
/// Card drawn behind every thumbnail. Only shows as matting around a window
/// that is not the screen's shape, so it stays darker than the frame: empty
/// space should read as empty, not as a broken tile.
const CARD: Color32F = Color32F::new(0.09, 0.09, 0.11, 1.0);
/// A workspace's miniature screen, behind its windows. A shade lighter than
/// the cards so empty desktop space reads as part of the workspace.
const PANEL: Color32F = Color32F::new(0.12, 0.12, 0.15, 0.92);
/// Outline on every unselected cell. Without it the cards vanish into the
/// backdrop and the grid reads as loose floating rectangles.
const FRAME_IDLE: Color32F = Color32F::new(0.28, 0.28, 0.34, 1.0);

/// Height of the label strip along the bottom of a cell, in logical pixels.
/// The strip is drawn over the thumbnail rather than beside it so the grid
/// geometry — and its tests — stay about the windows, not the chrome.
const LABEL_HEIGHT: i32 = 28;
/// Title text size within the strip.
const LABEL_TEXT_PX: f32 = 13.0;
/// Icon edge length within the strip.
const LABEL_ICON: i32 = 18;
/// Gap around and between the strip's contents.
const LABEL_PAD: i32 = 8;
/// Strip background. Translucent so the thumbnail still reads underneath.
const LABEL_BG: [f32; 4] = [0.05, 0.05, 0.07, 0.78];
/// Title colour — slightly off-white so it does not glare at small sizes.
const LABEL_FG: [f32; 3] = [0.88, 0.88, 0.92];

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

/// Number of columns to lay `count` thumbnails out in.
///
/// Cells keep the screen's own proportions (see [`cell_rects`]), so a
/// thumbnail's size is capped by the column count *and* the row count alike —
/// the aspect ratio cancels out. The best grid is therefore the one with the
/// smallest `max(cols, rows)`, and among ties the widest one. 10 windows come
/// out as 4 × 3, whose cells are 26% larger than a 5 × 2's.
pub fn grid_columns(count: usize) -> usize {
    (1..=count)
        .min_by_key(|&cols| (cols.max(count.div_ceil(cols)), count.div_ceil(cols)))
        .unwrap_or(0)
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

    let cols = grid_columns(count);
    let rows = count.div_ceil(cols);
    // The largest cell that both fits the grid into `area` and keeps the
    // screen's proportions. Stretching the cells to fill `area` instead would
    // letterbox every full-screen window inside its card. `max(1)` keeps a
    // degenerate cell (very many windows on a small output) renderable.
    let fit_w = ((area.size.w - gap * (cols as i32 - 1)) / cols as i32).max(1);
    let fit_h = ((area.size.h - gap * (rows as i32 - 1)) / rows as i32).max(1);
    let (cell_w, cell_h) = if fit_w * area.size.h <= fit_h * area.size.w {
        (fit_w, (fit_w * area.size.h / area.size.w).max(1))
    } else {
        ((fit_h * area.size.w / area.size.h).max(1), fit_h)
    };

    // The grid no longer fills `area`, so centre it in the slack it leaves.
    let grid_h = rows as i32 * cell_h + (rows as i32 - 1) * gap;
    let top = area.loc.y + ((area.size.h - grid_h) / 2).max(0);

    (0..count)
        .map(|index| {
            let row = index / cols;
            let col = index % cols;
            let in_row = (count - row * cols).min(cols) as i32;
            let row_width = in_row * cell_w + (in_row - 1) * gap;
            let x = area.loc.x + (area.size.w - row_width) / 2 + col as i32 * (cell_w + gap);
            let y = top + row as i32 * (cell_h + gap);
            Rectangle::new((x, y).into(), (cell_w, cell_h).into())
        })
        .collect()
}

/// The largest rectangle of `aspect` (width / height) that fits inside `slot`,
/// centred.
///
/// Used to fit a workspace's miniature screen into its grid slot when that
/// workspace lives on a monitor with a different shape from the one showing
/// the overview.
pub fn fit(slot: Rectangle<i32, Logical>, aspect: f64) -> Rectangle<i32, Logical> {
    if !aspect.is_finite() || aspect <= 0.0 || slot.size.w <= 0 || slot.size.h <= 0 {
        return slot;
    }
    let slot_aspect = slot.size.w as f64 / slot.size.h as f64;
    let (width, height) = if aspect >= slot_aspect {
        (
            slot.size.w,
            ((slot.size.w as f64 / aspect).round() as i32).max(1),
        )
    } else {
        (
            ((slot.size.h as f64 * aspect).round() as i32).max(1),
            slot.size.h,
        )
    };
    Rectangle::new(
        (
            slot.loc.x + (slot.size.w - width) / 2,
            slot.loc.y + (slot.size.h - height) / 2,
        )
            .into(),
        (width, height).into(),
    )
}

/// Map `window` (global logical coordinates on `screen`) into `panel`, the
/// miniature of that screen, then pull it in by `inset` on every side so the
/// outline drawn around it stays inside its real footprint.
///
/// Both edges are scaled independently and rounded, so two windows that touch
/// on the real screen keep the same spacing in the miniature (to within a
/// pixel), the result keeps the window's proportions, and it never leaves
/// `panel`.
pub fn place_in_panel(
    panel: Rectangle<i32, Logical>,
    screen: Rectangle<i32, Logical>,
    window: Rectangle<i32, Logical>,
    inset: i32,
) -> Rectangle<i32, Logical> {
    if screen.size.w <= 0 || screen.size.h <= 0 {
        return panel;
    }
    let sx = panel.size.w as f64 / screen.size.w as f64;
    let sy = panel.size.h as f64 / screen.size.h as f64;
    let map = |value: i32, origin: i32, scale: f64, base: i32, extent: i32| {
        (base + ((value - origin) as f64 * scale).round() as i32).clamp(base, base + extent)
    };
    let x0 = map(window.loc.x, screen.loc.x, sx, panel.loc.x, panel.size.w);
    let x1 = map(
        window.loc.x + window.size.w,
        screen.loc.x,
        sx,
        panel.loc.x,
        panel.size.w,
    );
    let y0 = map(window.loc.y, screen.loc.y, sy, panel.loc.y, panel.size.h);
    let y1 = map(
        window.loc.y + window.size.h,
        screen.loc.y,
        sy,
        panel.loc.y,
        panel.size.h,
    );
    // Give up the inset before the window: a card must stay at least a pixel.
    let shrink = |lo: i32, hi: i32| {
        let inset = inset.clamp(0, ((hi - lo - 1) / 2).max(0));
        (lo + inset, (hi - lo - 2 * inset).max(1))
    };
    let (x, w) = shrink(x0, x1);
    let (y, h) = shrink(y0, y1);
    // Taking the same inset off both axes changes the shape: a short, wide
    // card ends up proportionally wider than its window, and the window, drawn
    // undistorted inside it, leaves the outline showing as a thicker band down
    // both sides. Trim the card back to the window's own proportions so the
    // outline hugs it evenly. A pixel of difference is just rounding and is
    // left alone, so cards that line up on screen still line up here.
    let card = Rectangle::new((x, y).into(), (w, h).into());
    let fitted = fit(card, window.size.w as f64 / window.size.h as f64);
    if card.size.w - fitted.size.w > 1 || card.size.h - fitted.size.h > 1 {
        fitted
    } else {
        card
    }
}

/// Where `nav` moves the selection among `cards` laid out freely in 2D.
///
/// Tab/Shift+Tab walk the list and wrap. An arrow picks the nearest card whose
/// centre lies in that direction (within a cone, so a card far off to the side
/// does not count), favouring ones straight ahead, and stays put when there is
/// none.
pub fn nav_spatial(cards: &[Rectangle<i32, Logical>], selected: usize, nav: OverviewNav) -> usize {
    let count = cards.len();
    if count == 0 {
        return 0;
    }
    let selected = selected.min(count - 1);
    let center = |rect: &Rectangle<i32, Logical>| {
        (
            rect.loc.x as f64 + rect.size.w as f64 / 2.0,
            rect.loc.y as f64 + rect.size.h as f64 / 2.0,
        )
    };
    let (cx, cy) = center(&cards[selected]);
    let (dx, dy) = match nav {
        OverviewNav::Next => return (selected + 1) % count,
        OverviewNav::Prev => return (selected + count - 1) % count,
        OverviewNav::Left => (-1.0, 0.0),
        OverviewNav::Right => (1.0, 0.0),
        OverviewNav::Up => (0.0, -1.0),
        OverviewNav::Down => (0.0, 1.0),
    };
    cards
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != selected)
        .filter_map(|(index, rect)| {
            let (x, y) = center(rect);
            let ahead = (x - cx) * dx + (y - cy) * dy;
            let aside = ((x - cx) * dy - (y - cy) * dx).abs();
            // Only cards within a cone around the arrow count: "up" must not
            // jump to something far off to the side that is barely higher.
            (ahead > 0.5 && aside <= 2.0 * ahead).then_some((index, ahead + 2.0 * aside))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
        .unwrap_or(selected)
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
    /// One miniature screen per non-empty workspace, in output-local logical
    /// coordinates.
    panels: Vec<Rectangle<i32, Logical>>,
    panel_ids: Vec<Id>,
    /// Each window's card, index-aligned with `items`: its real place on its
    /// workspace, scaled into that workspace's panel. These are also the
    /// pointer hit targets.
    thumbs: Vec<Rectangle<i32, Logical>>,
    /// Item indices topmost first (floating windows above tiled ones), for
    /// drawing and for picking under the pointer.
    z_order: Vec<usize>,
    pub selected: usize,
    /// Cell currently hovered by the pointer, if any.
    pub hovered: Option<usize>,
    /// One pre-composited label strip per item, index-aligned with `items`.
    /// Built once when the grid opens: rasterising a title costs far more than
    /// a frame's budget allows, and neither the titles nor the cells move while
    /// the grid is up.
    labels: Vec<Option<Label>>,
    /// The output the grid is drawn on: the focused one when it opened.
    pub output: Output,
    backdrop_id: Id,
    selection_id: Id,
    cell_ids: Vec<CellIds>,
}

/// The two static quads for one cell. Their IDs must stay stable while the grid
/// is up or the damage tracker redraws every cell on every frame.
struct CellIds {
    frame: Id,
    card: Id,
}

/// A rendered label strip: background, icon and title already composited into
/// one buffer, so it costs a single render element per cell.
struct Label {
    buffer: MemoryRenderBuffer,
    /// Logical size the buffer should be drawn at, which is its pixel size
    /// divided back down by the output scale it was rasterised for.
    size: smithay::utils::Size<i32, Logical>,
    /// The title this was composited from, so [`Beewm::refresh_overview_labels`]
    /// can tell when it has gone stale.
    title: String,
}

/// Composite the strip for one window at `scale`, returning `None` when the
/// cell is too small to hold a legible one.
fn build_label(window: &Window, cell_width: i32, scale: f64) -> Option<Label> {
    let title = focused_window_title(window);
    let icon = appicon::icon_for_window(window, (LABEL_ICON as f64 * scale).round() as u32);
    if title.is_empty() && icon.is_none() {
        return None;
    }

    // Everything is rasterised in physical pixels so the text stays crisp on a
    // scaled output, then handed back a logical size to be drawn at.
    let px = |logical: i32| (logical as f64 * scale).round() as i32;
    let (width, height) = (px(cell_width), px(LABEL_HEIGHT));
    if width <= 0 || height <= 0 {
        return None;
    }
    let mut canvas = text::Rgba::filled(width as usize, height as usize, LABEL_BG);

    let mut left = px(LABEL_PAD);
    if let Some(icon) = &icon {
        canvas.blend(icon, left, (height - icon.height as i32) / 2);
        left += icon.width as i32 + px(LABEL_PAD);
    }
    let text_px = LABEL_TEXT_PX * scale as f32;
    let max_text = width - left - px(LABEL_PAD);
    if let Some(text_height) = text::line_height(text_px) {
        text::draw_line(
            &mut canvas,
            left,
            (height - text_height) / 2,
            title.as_str(),
            text_px,
            max_text,
            LABEL_FG,
        );
    }

    Some(Label {
        title,
        buffer: MemoryRenderBuffer::from_slice(
            &canvas.pixels,
            Fourcc::Abgr8888,
            (width, height),
            1,
            Transform::Normal,
            None,
        ),
        size: (cell_width, LABEL_HEIGHT).into(),
    })
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

    /// Re-composite any label whose window has renamed itself since the grid
    /// opened. Rasterising is far too slow to do per frame, but a title only
    /// changes when the client says so, and the comparison is a string compare
    /// per window per loop turn.
    ///
    /// Called from the same place as [`Beewm::tick_overview`].
    pub fn refresh_overview_labels(&mut self) {
        let Some(overview) = self.overview.as_mut() else {
            return;
        };
        let scale = overview.output.current_scale().fractional_scale();
        let mut changed = false;
        for (index, item) in overview.items.iter().enumerate() {
            let Some(width) = overview.thumbs.get(index).map(|thumb| thumb.size.w) else {
                continue;
            };
            let title = focused_window_title(&item.window);
            let stale = overview
                .labels
                .get(index)
                .map(|label| label.as_ref().map(|label| label.title.as_str()) != Some(&title))
                .unwrap_or(false);
            if !stale {
                continue;
            }
            overview.labels[index] = build_label(&item.window, width, scale);
            changed = true;
        }
        if changed {
            self.needs_render = true;
        }
    }

    /// The global rectangle of the screen `workspace` is shown on, without any
    /// bar strips: what each overview panel is a miniature of.
    fn workspace_screen(&self, workspace: usize) -> Option<Rectangle<i32, Logical>> {
        let output_idx = self.workspaces.get(workspace)?.output;
        let output = &self.outputs.get(output_idx)?.output;
        // The part of the screen windows can occupy: the output minus the
        // strips layer-shell surfaces reserve (beebar). Miniaturising the whole
        // output left that strip as an empty grey band on top of every panel.
        self.floating_usable_rect_for(output)
            .filter(|usable| usable.size.w > 0 && usable.size.h > 0)
            .or_else(|| self.space.output_geometry(output))
    }

    /// Where every window of `workspace` sits on its screen, in global logical
    /// coordinates, whether or not the workspace is visible right now. Tiled
    /// windows (fullscreen ones included, at the slot they return to) come from
    /// the layout itself; floating ones from their stored geometry.
    fn workspace_window_rects(
        &self,
        workspace: usize,
    ) -> HashMap<WlSurface, Rectangle<i32, Logical>> {
        let mut rects = HashMap::new();
        let Some(ws) = self.workspaces.get(workspace) else {
            return rects;
        };
        let roots: Vec<WlSurface> = ws
            .windows
            .iter()
            .filter_map(Self::window_root_surface)
            .collect();
        for root in &roots {
            if let Some(floating) = self.floating_windows.get(root) {
                rects.insert(
                    root.clone(),
                    Rectangle::new(floating.position, floating.size),
                );
            }
        }
        let tiled: Vec<WlSurface> = roots
            .into_iter()
            .filter(|root| !self.is_root_floating(root))
            .collect();
        let usable = self
            .outputs
            .get(ws.output)
            .and_then(|ctx| self.tiling_usable_geometry_for(&ctx.output));
        if let Some(usable) = usable {
            for (root, geo) in self.layout_manager.geometries(workspace, &usable, &tiled) {
                if tiled.contains(&root) {
                    rects.insert(root, self.tiled_content_rect(geo));
                }
            }
        }
        rects
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

        // One miniature screen per non-empty workspace, laid out in a grid.
        // Each window is then drawn inside its workspace's panel exactly where
        // it sits on the real screen, so windows that share a workspace stay
        // side by side instead of each taking a grid slot of its own.
        let mut groups: Vec<usize> = items.iter().map(|item| item.workspace).collect();
        groups.dedup();
        let cells = cell_rects(groups.len(), area, GAP);
        let mut panels = Vec::with_capacity(groups.len());
        let mut thumbs = Vec::with_capacity(items.len());
        let mut floating = Vec::with_capacity(items.len());
        for (workspace, cell) in groups.iter().zip(&cells) {
            let screen = self.workspace_screen(*workspace).unwrap_or(region);
            let panel = fit(*cell, screen.size.w as f64 / screen.size.h.max(1) as f64);
            panels.push(panel);
            let rects = self.workspace_window_rects(*workspace);
            for item in items.iter().filter(|item| item.workspace == *workspace) {
                let root = Self::window_root_surface(&item.window);
                let rect = root
                    .as_ref()
                    .and_then(|root| rects.get(root).copied())
                    .unwrap_or_else(|| {
                        // No layout slot (e.g. mid-map): centre it at its size.
                        let size = item.window.geometry().size;
                        Rectangle::new(
                            screen.loc
                                + Point::from((
                                    (screen.size.w - size.w) / 2,
                                    (screen.size.h - size.h) / 2,
                                )),
                            size,
                        )
                    });
                thumbs.push(place_in_panel(panel, screen, rect, FRAME));
                floating.push(
                    root.as_ref()
                        .map(|root| self.is_root_floating(root))
                        .unwrap_or(false),
                );
            }
        }

        // Tab walks each workspace in reading order, not insertion order.
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_by_key(|&index| {
            (
                items[index].workspace,
                thumbs[index].loc.y,
                thumbs[index].loc.x,
            )
        });
        let mut items_by_order: Vec<Option<OverviewItem>> = items.into_iter().map(Some).collect();
        let items: Vec<OverviewItem> = order
            .iter()
            .filter_map(|&index| items_by_order[index].take())
            .collect();
        let thumbs: Vec<_> = order.iter().map(|&index| thumbs[index]).collect();
        let floating: Vec<bool> = order.iter().map(|&index| floating[index]).collect();
        // Floating windows sit above the tiled ones, as on the real screen;
        // this decides both drawing and which card the pointer lands on.
        let mut z_order: Vec<usize> = (0..items.len()).collect();
        z_order.sort_by_key(|&index| !floating[index]);

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

        let label_scale = output.current_scale().fractional_scale();
        let labels = items
            .iter()
            .zip(&thumbs)
            .map(|(item, thumb)| build_label(&item.window, thumb.size.w, label_scale))
            .collect();

        self.overview = Some(Overview {
            labels,
            thumbs,
            cell_ids: (0..items.len())
                .map(|_| CellIds {
                    frame: Id::new(),
                    card: Id::new(),
                })
                .collect(),
            items,
            panel_ids: panels.iter().map(|_| Id::new()).collect(),
            panels,
            z_order,
            selected,
            hovered: Some(selected),
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
        // The item list is a snapshot from when the grid opened, so the
        // selected window may have closed since. Releasing Super onto a dead
        // card used to do nothing at all; fall through to the next live one so
        // the release always lands somewhere.
        let live = overview
            .items
            .iter()
            .cycle()
            .skip(overview.selected)
            .take(overview.items.len())
            .find(|item| item.window.alive());
        let Some(item) = live else {
            return;
        };
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
        let target = nav_spatial(&overview.thumbs, overview.selected, nav);
        self.set_overview_selection(target);
    }

    /// Hover-select while the grid is up. Returns `true` when the motion was
    /// consumed, so the pointer never reaches a client behind the grid.
    pub(crate) fn overview_pointer_moved(&mut self, pos: Point<f64, Logical>) -> bool {
        let Some(overview) = self.overview.as_mut() else {
            return false;
        };
        let Some(region) = self.space.output_geometry(&overview.output) else {
            return true;
        };
        let local = pos - region.loc.to_f64();
        let hovered = overview
            .z_order
            .iter()
            .copied()
            .find(|&index| overview.thumbs[index].to_f64().contains(local));
        overview.hovered = hovered;
        if let Some(idx) = hovered
            && overview.selected != idx
        {
            overview.selected = idx;
            self.needs_render = true;
        }
        true
    }

    /// A pointer button while the grid is up picks the hovered thumbnail.
    /// Clicking empty space outside any window card dismisses the grid
    /// without activating a new window. Returns `true` when the click was consumed.
    pub(crate) fn overview_pointer_pressed(&mut self) -> bool {
        let Some(overview) = self.overview.as_ref() else {
            return false;
        };
        let activate = overview.hovered.is_some();
        self.close_overview(activate);
        true
    }
}

/// Everything one window card draws, kept together so a floating window stacks
/// over its tiled neighbours as a whole, outline and label included.
struct WindowLayer<R: Renderer> {
    label: Option<MemoryRenderBufferRenderElement<R>>,
    thumbnail: Vec<WindowElement<R>>,
    frame: Option<SolidColorRenderElement>,
    card: SolidColorRenderElement,
}

/// Every layer of the open overview, kept apart so the stacking order lives in
/// one place ([`OverviewElements::into_ordered`]) instead of being re-derived
/// by each backend.
pub(crate) struct OverviewElements<R: Renderer> {
    /// Topmost window first.
    windows: Vec<WindowLayer<R>>,
    panels: Vec<SolidColorRenderElement>,
    shadows: Vec<PixelShaderElement>,
    backdrop: Option<SolidColorRenderElement>,
}

impl<R: Renderer> Default for OverviewElements<R> {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
            panels: Vec::new(),
            shadows: Vec::new(),
            backdrop: None,
        }
    }
}

impl<R: Renderer> OverviewElements<R> {
    /// Flatten into one front-to-back list, topmost first.
    ///
    /// Per window: the label over the thumbnail, the outline *behind* the
    /// thumbnail (grown a few pixels past it, so what shows is a border), and
    /// the card behind that as a backstop for a window that has not drawn yet.
    /// Below all windows come the workspace panels, the shadows they cast, and
    /// the dimming backdrop last of all.
    pub fn into_ordered<T>(self) -> Vec<T>
    where
        T: From<MemoryRenderBufferRenderElement<R>>
            + From<PixelShaderElement>
            + From<WindowElement<R>>
            + From<SolidColorRenderElement>,
    {
        let mut elements: Vec<T> = Vec::new();
        for layer in self.windows {
            elements.extend(layer.label.map(T::from));
            elements.extend(layer.thumbnail.into_iter().map(T::from));
            elements.extend(layer.frame.map(T::from));
            elements.push(T::from(layer.card));
        }
        elements.extend(self.panels.into_iter().map(T::from));
        elements.extend(self.shadows.into_iter().map(T::from));
        elements.extend(self.backdrop.into_iter().map(T::from));
        elements
    }
}

/// Expand a rectangle by `by` on every side.
fn grow(rect: Rectangle<i32, Logical>, by: i32) -> Rectangle<i32, Logical> {
    Rectangle::new(
        (rect.loc.x - by, rect.loc.y - by).into(),
        (rect.size.w + by * 2, rect.size.h + by * 2).into(),
    )
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

/// Build the overview's render elements for `output`. See
/// [`OverviewElements::into_ordered`] for how they stack.
///
/// Returns empty lists when the grid is closed or `output` is not the one it
/// opened on, so the other outputs keep rendering their desktop normally.
pub(crate) fn overview_elements<R>(
    state: &Beewm,
    renderer: &mut R,
    output: &Output,
) -> OverviewElements<R>
where
    R: Renderer + ImportAll + ImportMem + BorrowMut<GlesRenderer>,
    R::TextureId: Texture + Clone + Send + 'static,
{
    let empty = OverviewElements::default;
    let Some(overview) = state.overview.as_ref() else {
        return empty();
    };
    if overview.output != *output {
        return empty();
    }
    let Some(region) = state.space.output_geometry(output) else {
        return empty();
    };
    let scale = Scale::from(output.current_scale().fractional_scale());

    // A window that closed while the grid is up keeps its slot — relaying the
    // grid out under the pointer would be worse — but nothing is drawn in it.
    let live: Vec<bool> = overview
        .items
        .iter()
        .map(|item| item.window.alive())
        .collect();
    let is_live = |index: usize| live.get(index).copied().unwrap_or(false);

    let mut windows = Vec::with_capacity(overview.items.len());
    for &index in &overview.z_order {
        if !is_live(index) {
            continue;
        }
        let (Some(item), Some(cell), Some(ids)) = (
            overview.items.get(index),
            overview.thumbs.get(index),
            overview.cell_ids.get(index),
        ) else {
            continue;
        };

        // `Fit` keeps the window whole and undistorted. The card is the
        // window's own rectangle scaled down, so it fills it exactly.
        let thumbnail = constrain_space_element::<R, Window, WindowElement<R>>(
            renderer,
            &item.window,
            cell.loc,
            1.0,
            scale,
            *cell,
            ConstrainBehavior {
                reference: ConstrainReference::Geometry,
                behavior: ConstrainScaleBehavior::Fit,
                align: ConstrainAlign::CENTER,
            },
        )
        .collect();

        // The selected outline is its own quad with its own ID rather than a
        // colour swap on the idle one: the damage tracker notices an element
        // appearing or moving, but not a colour change at identical geometry.
        let frame = if index == overview.selected {
            solid(
                overview.selection_id.clone(),
                grow(*cell, FRAME),
                scale,
                state.border_color_focused,
            )
        } else {
            solid(ids.frame.clone(), grow(*cell, FRAME), scale, FRAME_IDLE)
        };

        // Labels ride on the bottom edge of their card, over the thumbnail.
        let label = overview
            .labels
            .get(index)
            .and_then(Option::as_ref)
            .and_then(|label| {
                let top_left = Point::<i32, Logical>::from((
                    cell.loc.x,
                    cell.loc.y + cell.size.h - label.size.h,
                ));
                let location: Point<f64, Physical> = top_left.to_f64().to_physical(scale);
                MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    location,
                    &label.buffer,
                    None,
                    None,
                    Some(label.size),
                    Kind::Unspecified,
                )
                .inspect_err(|error| tracing::warn!("overview label element failed: {error:?}"))
                .ok()
            });

        windows.push(WindowLayer {
            label,
            thumbnail,
            frame: Some(frame),
            card: solid(ids.card.clone(), *cell, scale, CARD),
        });
    }

    // Each workspace is a miniature screen: a panel with a soft shadow, so the
    // windows on it read as one group.
    let panels = overview
        .panel_ids
        .iter()
        .zip(&overview.panels)
        .map(|(id, panel)| solid(id.clone(), *panel, scale, PANEL))
        .collect();
    let shadows = overview
        .panels
        .iter()
        .filter_map(|panel| overview_chrome::shadow_element(renderer, *panel))
        .collect();

    let backdrop = solid(
        overview.backdrop_id.clone(),
        Rectangle::from_size(region.size),
        scale,
        BACKDROP,
    );

    OverviewElements {
        windows,
        panels,
        shadows,
        backdrop: Some(backdrop),
    }
}
