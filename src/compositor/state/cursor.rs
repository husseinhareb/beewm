use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::utils::{Physical, Point};

use super::{ActiveGrab, Beewm};

impl Beewm {
    pub fn effective_cursor_icon(&self) -> Option<CursorIcon> {
        // Compositor-driven cursor (hovering borders, move grab) takes priority
        // over whatever the client has requested.
        if let Some(icon) = self.compositor_cursor_icon {
            return Some(icon);
        }
        match &self.cursor_status {
            CursorImageStatus::Hidden => None,
            CursorImageStatus::Named(icon) => Some(*icon),
            // Surface cursors are rendered separately; fall back to Default here
            // so the compositor's software cursor overlay is still visible.
            CursorImageStatus::Surface(_) => Some(CursorIcon::Default),
        }
    }

    pub fn set_cursor_status(&mut self, status: CursorImageStatus) {
        self.cursor_status = status;
        self.cursor_status_serial = self.cursor_status_serial.wrapping_add(1);
        self.needs_render = true;
    }

    /// Recalculate the compositor-owned cursor icon based on the current
    /// pointer position and grab state, and trigger a re-render if it changed.
    pub fn refresh_compositor_cursor(&mut self) {
        let icon = compute_compositor_cursor(self);
        if self.compositor_cursor_icon != icon {
            self.compositor_cursor_icon = icon;
            self.cursor_status_serial = self.cursor_status_serial.wrapping_add(1);
            self.needs_render = true;
        }
    }

    /// Build a themed software cursor element for the DRM backend.
    pub fn cursor_elements(
        &mut self,
        renderer: &mut GlesRenderer,
    ) -> Vec<MemoryRenderBufferRenderElement<GlesRenderer>> {
        let Some(icon) = self.effective_cursor_icon() else {
            return Vec::new();
        };

        let sprite = self.cursor_theme.sprite(icon);
        let location = Point::<f64, Physical>::from((
            self.pointer_location.x - sprite.hotspot.x as f64,
            self.pointer_location.y - sprite.hotspot.y as f64,
        ));

        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            location,
            &sprite.buffer,
            None,
            None,
            None,
            Kind::Cursor,
        ) {
            Ok(element) => vec![element],
            Err(error) => {
                tracing::warn!("Failed to build cursor element: {:?}", error);
                Vec::new()
            }
        }
    }
}

/// Determine the compositor-driven cursor icon for the current pointer
/// position and grab state. Returns `Some(icon)` when the compositor
/// itself should override the client cursor, `None` to fall through.
fn compute_compositor_cursor(state: &Beewm) -> Option<CursorIcon> {
    match &state.active_grab {
        Some(ActiveGrab::Resize(grab)) => return Some(grab.edges.cursor_icon()),
        Some(ActiveGrab::TiledResize(grab)) => return Some(grab.edges.cursor_icon()),
        Some(ActiveGrab::Move(_)) | Some(ActiveGrab::TiledSwap(_)) => {
            return Some(CursorIcon::Grabbing);
        }
        None => {}
    }

    let bw = state.config.border_width as i32;
    if bw == 0 {
        return None;
    }

    let px = state.pointer_location.x as i32;
    let py = state.pointer_location.y as i32;

    for window in state.space.elements() {
        // Fullscreen windows have no compositor-drawn borders.
        if state
            .fullscreen_window
            .as_ref()
            .map(|fs| fs == window)
            .unwrap_or(false)
        {
            continue;
        }

        let Some(root) = Beewm::window_root_surface(window) else {
            continue;
        };
        if !state.is_root_floating(&root) {
            continue;
        }

        let geo = match state.space.element_geometry(window) {
            Some(g) => g,
            None => continue,
        };

        let x = geo.loc.x;
        let y = geo.loc.y;
        let w = geo.size.w;
        let h = geo.size.h;

        // Outer bounding box that includes the border strip.
        let outer_x = x - bw;
        let outer_y = y - bw;
        let outer_w = w + bw * 2;
        let outer_h = h + bw * 2;

        if px < outer_x || px >= outer_x + outer_w || py < outer_y || py >= outer_y + outer_h {
            continue;
        }

        let on_left = px < x;
        let on_right = px >= x + w;
        let on_top = py < y;
        let on_bottom = py >= y + h;

        if on_left || on_right || on_top || on_bottom {
            return Some(match (on_top, on_bottom, on_left, on_right) {
                (true, _, true, _) => CursorIcon::NwResize,
                (true, _, _, true) => CursorIcon::NeResize,
                (_, true, true, _) => CursorIcon::SwResize,
                (_, true, _, true) => CursorIcon::SeResize,
                (true, _, _, _) => CursorIcon::NResize,
                (_, true, _, _) => CursorIcon::SResize,
                (_, _, true, _) => CursorIcon::WResize,
                _ => CursorIcon::EResize,
            });
        }
    }

    None
}
