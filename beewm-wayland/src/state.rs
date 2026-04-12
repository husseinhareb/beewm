use beewm_core::config::Config;
use beewm_core::layout::master_stack::MasterStack;
use beewm_core::layout::Layout;
use beewm_core::model::window::Geometry;
use beewm_core::model::workspace::Workspace;

use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Color32F;
use smithay::desktop::{Space, Window};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatState};
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;

/// Wrapper passed to calloop; holds compositor state + display handle.
pub struct CalloopData {
    pub state: Beewm,
    pub display_handle: DisplayHandle,
}

/// The main compositor state.
#[allow(dead_code)]
pub struct Beewm {
    pub display_handle: DisplayHandle,
    pub running: bool,
    pub config: Config,
    pub start_time: std::time::Instant,

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: Option<DmabufGlobal>,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,
    pub cursor_status: CursorImageStatus,

    // Pointer
    pub pointer_location: Point<f64, Logical>,
    pub cursor_id: Id,
    pub cursor_serial: u64,

    // Session (for VT switching in TTY mode)
    pub session: Option<Box<dyn std::any::Any>>,

    // Desktop management
    pub space: Space<Window>,
    pub layout: Box<dyn Layout>,
    pub workspaces: Vec<Workspace>,
    pub workspace_windows: Vec<Vec<Window>>,
    pub active_workspace: usize,
}

impl Beewm {
    pub fn new(display: &Display<Self>, config: Config) -> Self {
        let display_handle = display.handle();

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let dmabuf_state = DmabufState::new();
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "beewm");

        // Initialize keyboard and pointer on the seat
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("Failed to add keyboard");
        seat.add_pointer();

        let num_ws = config.num_workspaces;
        let layout = Box::new(MasterStack {
            master_ratio: config.master_ratio,
        });

        Self {
            display_handle,
            running: true,
            config,
            start_time: std::time::Instant::now(),
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            layer_shell_state,
            shm_state,
            data_device_state,
            primary_selection_state,
            dmabuf_state,
            dmabuf_global: None,
            seat_state,
            seat,
            cursor_status: CursorImageStatus::default_named(),
            pointer_location: Point::from((0.0, 0.0)),
            cursor_id: Id::new(),
            cursor_serial: 0,
            session: None,
            space: Space::default(),
            layout,
            workspaces: (0..num_ws).map(Workspace::new).collect(),
            workspace_windows: (0..num_ws).map(|_| Vec::new()).collect(),
            active_workspace: 0,
        }
    }

    /// Build border render elements for all visible windows.
    pub fn border_elements(&self) -> Vec<SolidColorRenderElement> {
        let bw = self.config.border_width as i32;
        if bw == 0 {
            return Vec::new();
        }

        let focused_surface = self
            .seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus());

        let focused_color = hex_to_color32f(self.config.border_color_focused);
        let unfocused_color = hex_to_color32f(self.config.border_color_unfocused);

        let mut elements = Vec::new();

        for window in self.space.elements() {
            let geo = match self.space.element_geometry(window) {
                Some(g) => g,
                None => continue,
            };

            let is_focused = window
                .toplevel()
                .and_then(|tl| {
                    focused_surface
                        .as_ref()
                        .map(|fs| *fs == *tl.wl_surface())
                })
                .unwrap_or(false);

            let color = if is_focused {
                focused_color
            } else {
                unfocused_color
            };

            // Window content is at geo.loc with size geo.size.
            // Borders are drawn OUTSIDE the content area.
            let x = geo.loc.x - bw;
            let y = geo.loc.y - bw;
            let w = geo.size.w + bw * 2;
            let h = geo.size.h + bw * 2;

            let commit = smithay::backend::renderer::utils::CommitCounter::default();

            // Top border
            elements.push(SolidColorRenderElement::new(
                Id::new(),
                Rectangle::new((x, y).into(), (w, bw).into()),
                commit,
                color,
                Kind::Unspecified,
            ));
            // Bottom border
            elements.push(SolidColorRenderElement::new(
                Id::new(),
                Rectangle::new((x, y + h - bw).into(), (w, bw).into()),
                commit,
                color,
                Kind::Unspecified,
            ));
            // Left border
            elements.push(SolidColorRenderElement::new(
                Id::new(),
                Rectangle::new((x, y + bw).into(), (bw, h - bw * 2).into()),
                commit,
                color,
                Kind::Unspecified,
            ));
            // Right border
            elements.push(SolidColorRenderElement::new(
                Id::new(),
                Rectangle::new((x + w - bw, y + bw).into(), (bw, h - bw * 2).into()),
                commit,
                color,
                Kind::Unspecified,
            ));
        }

        elements
    }

    /// Build a simple software cursor element (used by DRM backend).
    pub fn cursor_elements(&self) -> Vec<SolidColorRenderElement> {
        let commit = smithay::backend::renderer::utils::CommitCounter::from(self.cursor_serial as usize);
        vec![SolidColorRenderElement::new(
            self.cursor_id.clone(),
            Rectangle::new(
                (self.pointer_location.x as i32, self.pointer_location.y as i32).into(),
                (16, 16).into(),
            ),
            commit,
            Color32F::new(1.0, 1.0, 1.0, 1.0),
            Kind::Unspecified,
        )]
    }

    /// Re-tile all windows in the space using the current layout.
    pub fn relayout(&mut self) {
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };

        let output_geo = self.space.output_geometry(&output).unwrap();
        let gap = self.config.gap as i32;
        let bw = self.config.border_width as i32;

        let usable = Geometry::new(
            output_geo.loc.x + gap,
            output_geo.loc.y + gap,
            (output_geo.size.w - gap * 2).max(0) as u32,
            (output_geo.size.h - gap * 2).max(0) as u32,
        );

        let windows = &self.workspace_windows[self.active_workspace];
        let count = windows.len();

        if count == 0 {
            return;
        }

        let geos = self.layout.apply(&usable, count);

        for (window, geo) in windows.iter().zip(geos.iter()) {
            let x = geo.x + gap;
            let y = geo.y + gap;
            let w = (geo.width as i32 - gap * 2 - bw * 2).max(1);
            let h = (geo.height as i32 - gap * 2 - bw * 2).max(1);

            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(Size::from((w, h)));
                });
                toplevel.send_pending_configure();
            }
            self.space.map_element(window.clone(), (x, y), false);
        }
    }

    /// Switch to a different workspace by index.
    pub fn switch_workspace(&mut self, idx: usize) {
        if idx >= self.workspaces.len() || idx == self.active_workspace {
            return;
        }

        tracing::info!(
            "Switching workspace {} -> {}",
            self.active_workspace + 1,
            idx + 1
        );

        // Unmap all windows from the current workspace
        for window in &self.workspace_windows[self.active_workspace] {
            self.space.unmap_elem(window);
        }

        self.active_workspace = idx;

        // Map all windows from the target workspace
        for window in &self.workspace_windows[self.active_workspace] {
            self.space.map_element(window.clone(), (0, 0), false);
        }

        self.relayout();

        // Focus the active window on the new workspace
        let ws = &self.workspaces[self.active_workspace];
        if let Some(focus_idx) = ws.focused_idx {
            if let Some(window) = self.workspace_windows[self.active_workspace].get(focus_idx) {
                if let Some(toplevel) = window.toplevel() {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    let keyboard = self.seat.get_keyboard().unwrap();
                    keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                }
            }
        } else {
            // No windows — clear keyboard focus
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        }
    }

    /// Move the focused window to another workspace.
    pub fn move_to_workspace(&mut self, target: usize) {
        if target >= self.workspaces.len() || target == self.active_workspace {
            return;
        }

        let ws = &self.workspaces[self.active_workspace];
        let focus_idx = match ws.focused_idx {
            Some(i) => i,
            None => return,
        };

        let current = self.active_workspace;
        if focus_idx >= self.workspace_windows[current].len() {
            return;
        }

        // Remove window from current workspace
        let window = self.workspace_windows[current].remove(focus_idx);
        self.workspaces[current].remove_window(focus_idx);

        // Unmap from space (it's being moved away from the visible workspace)
        self.space.unmap_elem(&window);

        // Add to target workspace
        self.workspace_windows[target].push(window);
        self.workspaces[target].add_window();

        tracing::info!(
            "Moved window from workspace {} to {}",
            current + 1,
            target + 1
        );

        self.relayout();

        // Focus next window on current workspace if any
        let ws = &self.workspaces[self.active_workspace];
        if let Some(focus_idx) = ws.focused_idx {
            if let Some(window) = self.workspace_windows[self.active_workspace].get(focus_idx) {
                if let Some(toplevel) = window.toplevel() {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    let keyboard = self.seat.get_keyboard().unwrap();
                    keyboard.set_focus(self, Some(toplevel.wl_surface().clone()), serial);
                }
            }
        } else {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let keyboard = self.seat.get_keyboard().unwrap();
            keyboard.set_focus(self, Option::<WlSurface>::None, serial);
        }
    }
}

/// Convert a 0xRRGGBB hex color to smithay's Color32F (with alpha=1.0).
fn hex_to_color32f(hex: u32) -> Color32F {
    let r = ((hex >> 16) & 0xFF) as f32 / 255.0;
    let g = ((hex >> 8) & 0xFF) as f32 / 255.0;
    let b = (hex & 0xFF) as f32 / 255.0;
    Color32F::new(r, g, b, 1.0)
}

/// Per-client state required by smithay.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: smithay::reexports::wayland_server::backend::ClientId) {}
    fn disconnected(
        &self,
        _client_id: smithay::reexports::wayland_server::backend::ClientId,
        _reason: smithay::reexports::wayland_server::backend::DisconnectReason,
    ) {
    }
}
