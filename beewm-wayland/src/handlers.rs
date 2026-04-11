use smithay::delegate_compositor;
use smithay::delegate_data_device;
use smithay::delegate_layer_shell;
use smithay::delegate_output;
use smithay::delegate_primary_selection;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_decoration;
use smithay::delegate_xdg_shell;
use smithay::desktop::layer_map_for_output;
use smithay::desktop::LayerSurface as DesktopLayerSurface;
use smithay::desktop::Window;
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface, WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;

use crate::state::{Beewm, ClientState};

impl CompositorHandler for Beewm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Handle xdg toplevel commits
        if let Some(window) = self
            .space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == surface)
                    .unwrap_or(false)
            })
            .cloned()
        {
            window.on_commit();
        }

        // Handle layer surface commits
        if let Some(output) = self.space.outputs().next().cloned() {
            let mut layer_map = layer_map_for_output(&output);
            let is_layer = layer_map
                .layers()
                .any(|l| l.wl_surface() == surface);
            if is_layer {
                layer_map.arrange();
            }
        }
    }
}

impl BufferHandler for Beewm {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for Beewm {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for Beewm {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface);
        self.space.map_element(window.clone(), (0, 0), true);

        let ws_idx = self.active_workspace;
        self.workspace_windows[ws_idx].push(window);
        self.workspaces[ws_idx].add_window();

        self.relayout();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Find which workspace owns this window and remove it
        let target_surface = surface.wl_surface();
        for ws_idx in 0..self.workspace_windows.len() {
            if let Some(pos) = self.workspace_windows[ws_idx].iter().position(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == target_surface)
                    .unwrap_or(false)
            }) {
                self.workspace_windows[ws_idx].remove(pos);
                self.workspaces[ws_idx].remove_window(pos);
                break;
            }
        }

        self.relayout();
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        // Send initial configure so the popup can render
        let _ = surface.send_configure();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl SeatHandler for Beewm {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
}

impl OutputHandler for Beewm {
    fn output_bound(&mut self, _output: Output, _wl_output: WlOutput) {}
}

impl WlrLayerShellHandler for Beewm {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = output
            .as_ref()
            .and_then(|o| Output::from_resource(o))
            .or_else(|| self.space.outputs().next().cloned());

        let output = match output {
            Some(o) => o,
            None => return,
        };

        let desktop_layer = DesktopLayerSurface::new(surface, namespace);
        let mut layer_map = layer_map_for_output(&output);
        if let Err(e) = layer_map.map_layer(&desktop_layer) {
            tracing::error!("Failed to map layer surface: {}", e);
        }
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        if let Some(output) = self.space.outputs().next().cloned() {
            let target = surface.wl_surface().clone();
            let mut layer_map = layer_map_for_output(&output);
            let layer = layer_map
                .layers()
                .find(|l| *l.wl_surface() == target)
                .cloned();
            if let Some(layer) = layer {
                layer_map.unmap_layer(&layer);
            }
        }
    }
}

impl SelectionHandler for Beewm {
    type SelectionUserData = ();
}

impl ClientDndGrabHandler for Beewm {}
impl ServerDndGrabHandler for Beewm {}

impl DataDeviceHandler for Beewm {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl PrimarySelectionHandler for Beewm {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl XdgDecorationHandler for Beewm {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
    }
}

delegate_compositor!(Beewm);
delegate_shm!(Beewm);
delegate_xdg_shell!(Beewm);
delegate_xdg_decoration!(Beewm);
delegate_layer_shell!(Beewm);
delegate_data_device!(Beewm);
delegate_primary_selection!(Beewm);
delegate_seat!(Beewm);
delegate_output!(Beewm);
