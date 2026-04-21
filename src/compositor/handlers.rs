use smithay::delegate_compositor;
use smithay::delegate_cursor_shape;
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::delegate_data_device;
use smithay::delegate_drm_syncobj;
use smithay::delegate_dmabuf;
use smithay::delegate_fractional_scale;
use smithay::delegate_layer_shell;
use smithay::delegate_output;
use smithay::delegate_presentation;
use smithay::delegate_primary_selection;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_single_pixel_buffer;
use smithay::delegate_viewporter;
use smithay::delegate_xdg_decoration;
use smithay::delegate_xdg_shell;
use smithay::desktop::{
    find_popup_root_surface, layer_map_for_output, LayerSurface as DesktopLayerSurface,
    PopupKeyboardGrab, PopupKind, PopupPointerGrab, Window, WindowSurfaceType,
};
use smithay::input::pointer::{CursorImageStatus, Focus};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState, get_parent};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::drm_syncobj::{DrmSyncobjHandler, DrmSyncobjState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::fractional_scale::FractionalScaleHandler;
use smithay::wayland::selection::data_device::{set_data_device_focus,
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{set_primary_focus,
    PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface, KeyboardInteractivity, WlrLayerShellHandler, WlrLayerShellState,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use super::state::{Beewm, ClientState};
use super::state::popup::should_map_toplevel_floating;

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

    fn new_surface(&mut self, surface: &WlSurface) {
        Beewm::install_explicit_sync_hook(surface);
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.popup_manager.commit(surface);

        // Process buffer attachment for the surface tree — required for
        // the renderer to see committed wl_buffer contents.
        on_commit_buffer_handler::<Self>(surface);

        // If this is the initial commit of a pending window, map it now.
        if let Some(pos) = self.pending_windows.iter().position(|w| {
            w.toplevel()
                .map(|t| t.wl_surface() == surface)
                .unwrap_or(false)
        }) {
            let window = self.pending_windows.remove(pos);
            let ws_idx = self.active_workspace;
            // Dialogs and fixed-size splash/loading windows should float
            // centered instead of being tiled or inheriting a (0, 0) origin.
            let should_float = should_map_toplevel_floating(&window);
            let split_target = self.focused_tiled_window_root(ws_idx);
            self.workspaces[ws_idx].add_window(window.clone());
            self.publish_workspace_state();
            self.track_window(&window);
            // Propagate the first commit through the window's surface tree.
            window.on_commit();
            if should_float {
                self.map_as_floating_centered(&window);
                self.relayout();
            } else {
                self.insert_tiled_window(ws_idx, &window, split_target.as_ref());
                self.relayout();
            }

            // Focus the new window
            if let Some(toplevel) = window.toplevel() {
                let wl_surface = toplevel.wl_surface().clone();
                self.set_keyboard_focus(Some(wl_surface));
            }
            self.space.raise_element(&window, true);
            self.needs_render = true;
            return;
        }

        // Route the commit through the matching mapped toplevel without
        // scanning the whole visible space on every subsurface commit.
        if let Some(window) = self.mapped_window_for_surface(surface) {
            window.on_commit();
            self.needs_render = true;
        }

        // Handle layer surface commits: arrange the layer map and, after the
        // configure is sent, grant keyboard focus when the surface requests it.
        let output = self.space.outputs().next().cloned();
        if let Some(output) = output {
            // Single borrow: find layer, arrange, read keyboard_interactivity.
            let (is_layer, focus_wl_surface) = {
                let mut lm = layer_map_for_output(&output);
                // Use layer_for_surface so subsurface commits (e.g. bar content
                // updates) also trigger re-renders, not just root-surface commits.
                let layer = lm
                    .layer_for_surface(
                        surface,
                        WindowSurfaceType::TOPLEVEL | WindowSurfaceType::SUBSURFACE,
                    )
                    .cloned();
                match layer {
                    Some(layer) => {
                        // arrange() sends the configure event to the layer surface.
                        lm.arrange();
                        let ki = smithay::wayland::compositor::with_states(
                            layer.wl_surface(),
                            |states| {
                                states
                                    .cached_state
                                    .get::<smithay::wayland::shell::wlr_layer::LayerSurfaceCachedState>()
                                    .current()
                                    .keyboard_interactivity
                            },
                        );
                        let focus = if ki != KeyboardInteractivity::None {
                            Some(layer.wl_surface().clone())
                        } else {
                            None
                        };
                        (true, focus)
                    }
                    None => (false, None),
                }
            };

            if is_layer {
                if let Some(wl_surface) = focus_wl_surface {
                    let keyboard = self.seat.get_keyboard().unwrap();
                    let already_focused = keyboard
                        .current_focus()
                        .as_ref()
                        .map(|f| *f == wl_surface)
                        .unwrap_or(false);
                    if !already_focused {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(self, Some(wl_surface), serial);
                        tracing::debug!("Layer surface focused after configure");
                    }
                }
                self.needs_render = true;
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
        // Send an initial tiled size up front so terminals can render their
        // first real frame at the target geometry instead of painting a blank
        // placeholder and immediately resizing on first commit.
        let initial_size = if surface.parent().is_none() {
            self.initial_toplevel_size(surface.wl_surface())
        } else {
            None
        };
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            state.size = initial_size;
        });
        surface.send_configure();
        let window = Window::new_wayland_window(surface);
        self.pending_windows.push(window);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // Remove windows that died before their first commit.
        let target_surface = surface.wl_surface();
        if let Some(pos) = self.pending_windows.iter().position(|w| {
            w.toplevel()
                .map(|t| t.wl_surface() == target_surface)
                .unwrap_or(false)
        }) {
            self.pending_windows.remove(pos);
            return;
        }

        // Find which workspace owns this window and remove it
        for ws_idx in 0..self.workspaces.len() {
            if let Some(pos) = self.workspaces[ws_idx].windows.iter().position(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == target_surface)
                    .unwrap_or(false)
            }) {
                let window = self.workspaces[ws_idx].remove_window(pos).unwrap();
                let should_restore_focus = if ws_idx == self.active_workspace {
                    match self
                        .seat
                        .get_keyboard()
                        .and_then(|keyboard| keyboard.current_focus())
                    {
                        Some(current_focus) => {
                            let mut root = current_focus;
                            while let Some(parent) = get_parent(&root) {
                                root = parent;
                            }
                            root == *target_surface
                        }
                        None => true,
                    }
                } else {
                    false
                };
                self.untrack_window_for_surface(target_surface);
                // Clean up fullscreen state if this was the fullscreen window.
                let was_fullscreen = self
                    .fullscreen_window
                    .as_ref()
                    .and_then(|w| w.toplevel())
                    .map(|t| t.wl_surface() == target_surface)
                    .unwrap_or(false);
                if was_fullscreen {
                    self.fullscreen_window = None;
                    // Remap siblings that were unmapped while fullscreen was active.
                    for sibling in &self.workspaces[ws_idx].windows {
                        if self.space.element_geometry(sibling).is_none() {
                            self.space.map_element(sibling.clone(), (0, 0), false);
                        }
                    }
                }
                // Clean up floating state if this was a floating window.
                self.floating_windows.remove(target_surface);
                self.remove_tiled_window(ws_idx, target_surface);
                self.space.unmap_elem(&window);
                self.publish_workspace_state();
                if ws_idx == self.active_workspace {
                    if should_restore_focus {
                        let focus = self.workspaces[self.active_workspace]
                            .focused_idx
                            .and_then(|focus_idx| {
                                self.workspaces[self.active_workspace].windows.get(focus_idx)
                            })
                            .and_then(|window| window.toplevel())
                            .map(|toplevel| toplevel.wl_surface().clone());
                        self.set_keyboard_focus(focus);
                    }
                    self.relayout();
                    self.needs_render = true;
                }
                break;
            }
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        self.configure_xdg_popup(&surface, positioner);

        // Send initial configure so the popup can render with constrained placement.
        if let Err(error) = surface.send_configure() {
            tracing::warn!("Failed to configure popup: {:?}", error);
        }

        // Track the popup so PopupManager can manage its lifetime and grabs.
        if let Err(error) = self.popup_manager.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("Failed to track popup: {:?}", error);
        }
        self.needs_render = true;
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let seat = match Seat::from_resource(&seat) {
            Some(s) => s,
            None => return,
        };
        let popup = PopupKind::Xdg(surface);
        let root = match find_popup_root_surface(&popup) {
            Ok(r) => r,
            Err(_) => return,
        };
        let grab = match self.popup_manager.grab_popup(root, popup, &seat, serial) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("Popup grab denied: {:?}", e);
                return;
            }
        };
        if let Some(pointer) = seat.get_pointer() {
            if !pointer.is_grabbed() {
                pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Clear);
            }
        }
        if let Some(keyboard) = seat.get_keyboard() {
            if !keyboard.is_grabbed() {
                keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
            }
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        self.configure_xdg_popup(&surface, positioner);
        surface.send_repositioned(token);
        self.needs_render = true;
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
        self.set_cursor_status(image);
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        self.note_keyboard_focus_change(focused);
        // Deliver the current clipboard/primary selection to the newly focused client.
        let client = focused.and_then(|s| s.client());
        set_data_device_focus::<Self>(&self.display_handle, seat, client.clone());
        set_primary_focus::<Self>(&self.display_handle, seat, client);
    }
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
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned());

        let output = match output {
            Some(o) => o,
            None => return,
        };

        let desktop_layer = DesktopLayerSurface::new(surface, namespace);
        let mut layer_map = layer_map_for_output(&output);
        if let Err(e) = layer_map.map_layer(&desktop_layer) {
            tracing::error!("Failed to map layer surface: {}", e);
            return;
        }
        // arrange() computes geometry and sets server_pending size, but does NOT
        // send the initial configure (it guards on initial_configure_sent being true).
        // We must call send_pending_configure() explicitly to send the initial configure;
        // without this the bar client waits forever and never draws.
        layer_map.arrange();
        if desktop_layer
            .layer_surface()
            .send_pending_configure()
            .is_none()
        {
            tracing::warn!("Layer surface had no pending configure after arrange");
        }
        self.needs_render = true;
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

        self.needs_render = true;

        // Restore keyboard focus to the active tiled window, if any.
        let focus = self.workspaces[self.active_workspace]
            .focused_idx
            .and_then(|i| self.workspaces[self.active_workspace].windows.get(i))
            .and_then(|w| w.toplevel())
            .map(|t| t.wl_surface().clone());
        self.set_keyboard_focus(focus);
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

// TabletSeatHandler is required by delegate_cursor_shape! even though we have
// no tablet hardware; the trait provides default no-op implementations.
impl TabletSeatHandler for Beewm {}

impl DrmSyncobjHandler for Beewm {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        self.drm_syncobj_state.as_mut()
    }
}

delegate_compositor!(Beewm);
delegate_cursor_shape!(Beewm);
delegate_shm!(Beewm);
delegate_xdg_shell!(Beewm);
delegate_xdg_decoration!(Beewm);
delegate_layer_shell!(Beewm);
delegate_data_device!(Beewm);
delegate_primary_selection!(Beewm);
delegate_seat!(Beewm);
delegate_output!(Beewm);
delegate_presentation!(Beewm);
delegate_viewporter!(Beewm);
delegate_fractional_scale!(Beewm);
delegate_single_pixel_buffer!(Beewm);
delegate_drm_syncobj!(Beewm);

impl FractionalScaleHandler for Beewm {}

impl DmabufHandler for Beewm {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        // Accept all dmabufs — actual import happens at render time.
        if let Err(error) = notifier.successful::<Beewm>() {
            tracing::warn!("Failed to acknowledge dmabuf import: {:?}", error);
        }
    }
}

delegate_dmabuf!(Beewm);
