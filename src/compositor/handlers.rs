use smithay::delegate_compositor;
use smithay::delegate_cursor_shape;
use smithay::delegate_pointer_constraints;
use smithay::delegate_relative_pointer;
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
use smithay::delegate_xdg_dialog;
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
use smithay::utils::{Logical, Point, Rectangle, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::pointer_constraints::{
    PointerConstraint, PointerConstraintsHandler, with_pointer_constraint,
};
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
    get_parent, send_surface_state, with_states,
};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::drm_syncobj::{DrmSyncobjHandler, DrmSyncobjState};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, with_fractional_scale};
use smithay::wayland::output::OutputHandler;
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
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::dialog::XdgDialogHandler;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::backend::renderer::{BufferType, buffer_type};
use super::diagnostics::BufferKind;
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use super::state::{Beewm, lookup_client_compositor_state, root_surface};
use super::state::popup::classify_toplevel_floating;

/// Classify the buffer a surface just committed (for the `beewm::dmabuf` /
/// `beewm::commit` traces). Reads `current()` because beewm's `commit()` runs
/// after smithay has already promoted the pending state.
fn committed_buffer_kind(surface: &WlSurface) -> BufferKind {
    with_states(surface, |states| {
        match states
            .cached_state
            .get::<SurfaceAttributes>()
            .current()
            .buffer
            .as_ref()
        {
            Some(BufferAssignment::NewBuffer(buffer)) => match buffer_type(buffer) {
                Some(BufferType::Shm) => BufferKind::Shm,
                Some(BufferType::Dma) => BufferKind::Dmabuf,
                Some(_) => BufferKind::Other,
                None => BufferKind::Other,
            },
            _ => BufferKind::None,
        }
    })
}

impl Beewm {
    fn prime_surface_scale_state(&self, surface: &WlSurface) {
        let Some(output) = self.space.outputs().next().cloned() else {
            return;
        };

        let scale = output.current_scale();
        let transform = output.current_transform();

        with_states(surface, |states| {
            // kitty's Wayland startup path is sensitive to the first configure
            // arriving before it sees an explicit scale=1 hint.
            if surface.version() >= 6 && scale.integer_scale() == 1 {
                surface.preferred_buffer_scale(1);
            }

            send_surface_state(surface, states, scale.integer_scale(), transform);
            with_fractional_scale(states, |fractional_scale| {
                fractional_scale.set_preferred_scale(scale.fractional_scale());
            });
        });
    }
}

impl CompositorHandler for Beewm {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        lookup_client_compositor_state(client)
            .expect("missing compositor client state for Wayland or XWayland client")
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        if !crate::compositor::runtime_flags::flags().explicit_sync_disabled {
            Beewm::install_explicit_sync_hook(surface);
        }
    }

    fn commit(&mut self, surface: &WlSurface) {
        // Diagnostic: aggregate per-root-surface commit rate, how promptly the
        // client responds to our frame callbacks, and whether it is on the
        // dmabuf fast path. See `diagnostics.rs` for the log format.
        //   RUST_LOG=beewm::commit=info,beewm::dmabuf=warn
        // A game committing far below the monitor refresh rate, with a *small*
        // callback-to-commit latency, means the compositor is throttling it; a
        // *large* latency means the client/GPU is the bottleneck. An shm count
        // > 0 means the client fell off the dmabuf path and will be slow.
        //
        // Gated on the targets being enabled so a production session (no
        // `RUST_LOG`) pays nothing — `committed_buffer_kind` takes a surface
        // lock we don't otherwise need on this path.
        if tracing::enabled!(target: "beewm::commit", tracing::Level::INFO)
            || tracing::enabled!(target: "beewm::dmabuf", tracing::Level::WARN)
        {
            let key = root_surface(surface).id().protocol_id();
            let cb_latency = self.last_frame_callbacks_sent_at.map(|t| t.elapsed());
            let buffer = committed_buffer_kind(surface);
            self.commit_tracker.record(key, cb_latency, buffer);
        }

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
            let split_target = self.focused_tiled_window_root(ws_idx);
            self.workspaces[ws_idx].add_window(window.clone());
            self.publish_workspace_state();
            self.track_window(&window);
            // Propagate the first commit through the window's surface tree so
            // that cached state (including min/max size set by the client in its
            // first-commit batch) is up-to-date before we decide whether to float.
            window.on_commit();
            // Dialogs and fixed-size splash/loading windows should float
            // centered instead of being tiled or inheriting a (0, 0) origin.
            let window_root = window
                .wl_surface()
                .map(|s| root_surface(&s.into_owned()))
                .unwrap_or_else(|| root_surface(surface));

            // Classify *this* window on its own merits. A normal app that merely
            // has a dialog child is never floated here — only the child window's
            // own parent/modal/size signals (or a recorded floating intent from
            // a parent_changed/modal_changed that arrived while pending) float it.
            let class = classify_toplevel_floating(&window);
            let intent_recorded = self.pending_should_float.remove(&window_root);
            let should_float = class.should_float || intent_recorded;
            let reason = if class.should_float {
                class.reason
            } else if intent_recorded {
                "deferred-parent/modal-intent"
            } else {
                "normal"
            };

            // Capture an authentication/modal dialog that currently holds focus
            // *before* we map this window, so a tiled parent mapping behind it
            // does not steal its keyboard focus.
            let auth_dialog_root = self.focused_auth_dialog_root();

            let parent_root = self
                .toplevel_parent_surface(&window)
                .map(|parent| parent.id().protocol_id());

            if should_float {
                self.map_as_floating_centered(&window);
                self.relayout();
            } else {
                self.insert_tiled_window(ws_idx, &window, split_target.as_ref());
                self.relayout();
            }

            // A fullscreen window owns the keyboard and the whole screen;
            // newly-mapped tiled windows that arrive while it's active must
            // not steal its focus, otherwise the user is left typing into an
            // invisible window underneath and directional focus gets stuck.
            let fullscreen_blocks_focus = self
                .fullscreen_window
                .as_ref()
                .map(|fs| fs != &window)
                .unwrap_or(false);
            // Keep keyboard focus on an existing modal/auth dialog when the
            // window we just mapped is a tiled parent appearing behind it.
            let keep_auth_dialog_focus = !should_float && auth_dialog_root.is_some();
            let mut focus_target = window_root.id().protocol_id();
            if !fullscreen_blocks_focus {
                if keep_auth_dialog_focus {
                    if let Some(root) = auth_dialog_root.clone() {
                        focus_target = root.id().protocol_id();
                    }
                } else if let Some(toplevel) = window.toplevel() {
                    let wl_surface = toplevel.wl_surface().clone();
                    self.set_keyboard_focus(Some(wl_surface));
                }
                self.space.raise_element(&window, true);
                // After raising the new window, push every floating element back
                // above the tiled stack so dialogs stay visible when a tiled window
                // opens on top of them.
                self.raise_floating_windows();
            }

            let placement = if should_float {
                self.floating_windows
                    .get(&window_root)
                    .map(|floating| Rectangle::new(floating.position, floating.size))
            } else {
                self.space.element_geometry(&window)
            };
            tracing::info!(
                target = "beewm::floating",
                id = window_root.id().protocol_id(),
                app_id = ?class.app_id,
                title = ?class.title,
                has_parent = class.has_parent,
                ?parent_root,
                is_modal = class.is_modal,
                known_dialog = class.known_dialog,
                layer = if should_float { "floating" } else { "tiled" },
                reason,
                ?placement,
                kept_auth_dialog_focus = keep_auth_dialog_focus,
                focus_target,
                "mapped new toplevel",
            );
            self.log_stacking_order("after toplevel map");

            self.needs_render = true;
            return;
        }

        // Route the commit through the matching mapped toplevel without
        // scanning the whole visible space on every subsurface commit.
        if let Some(window) = self.mapped_window_for_surface(surface) {
            window.on_commit();

            // If we previously sent a `size = None` configure to release this
            // window from its tiled dimensions, re-center it now that the client
            // has committed at its natural size.
            //
            // Do NOT re-set keyboard focus here: the initial-commit path already
            // focused the window. The natural-size recommit can arrive many
            // milliseconds later, and re-focusing at that point would steal
            // focus from whatever the user clicked on in the meantime.
            let root = root_surface(surface);
            if self.pending_float_centers.remove(&root) {
                if let Some((_, floating)) = self.centered_floating_data(&window) {
                    self.floating_windows.insert(root, floating);
                    self.relayout();
                    self.space.raise_element(&window, true);
                    self.raise_floating_windows();
                }
            }

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
                        .and_then(|target| target.wl_surface().map(|s| s.into_owned()))
                        .map(|focused| focused == wl_surface)
                        .unwrap_or(false);
                    if !already_focused {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(
                            self,
                            Some(super::focus_target::KeyboardFocusTarget::Wayland(wl_surface)),
                            serial,
                        );
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
        self.prime_surface_scale_state(surface.wl_surface());

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

    fn parent_changed(&mut self, surface: ToplevelSurface) {
        self.adopt_floating_dialog_state(&surface);
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        // Republish only when the title-changed surface is the focused one —
        // background tabs/clients renaming themselves shouldn't cause the
        // status bar to flicker through unrelated titles.
        let is_focused = self
            .active_workspace_focused_window()
            .and_then(|window| window.toplevel().map(|toplevel| toplevel.wl_surface().clone()))
            .as_ref()
            == Some(surface.wl_surface());
        if is_focused {
            self.request_focus_publish();
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, output: Option<WlOutput>) {
        let Some(window) = self.mapped_window_for_surface(surface.wl_surface()) else {
            surface.send_configure();
            return;
        };

        let already_fullscreen = self
            .fullscreen_window
            .as_ref()
            .map(|fullscreen| fullscreen == &window)
            .unwrap_or(false);
        if already_fullscreen {
            return;
        }
        if self.fullscreen_window.is_some() {
            self.restore_fullscreen();
        }

        let Some(output) = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.space.outputs().next().cloned())
        else {
            surface.send_configure();
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            surface.send_configure();
            return;
        };

        let ws_idx = self
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, workspace)| {
                workspace
                    .windows
                    .iter()
                    .any(|candidate| *candidate == window)
                    .then_some(ws_idx)
            })
            .unwrap_or(self.active_workspace);

        for sibling in &self.workspaces[ws_idx].windows {
            if *sibling != window {
                self.space.unmap_elem(sibling);
            }
        }

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.size = Some(output_geo.size);
        });
        surface.send_configure();
        self.space.map_element(window.clone(), output_geo.loc, true);
        self.fullscreen_window = Some(window.clone());
        self.set_keyboard_focus(Some(surface.wl_surface().clone()));
        self.needs_render = true;
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let is_current_fullscreen = self
            .fullscreen_window
            .as_ref()
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface() == surface.wl_surface())
            .unwrap_or(false);

        if is_current_fullscreen {
            self.restore_fullscreen();
        }
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
            self.pending_should_float.remove(target_surface);
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
                // Remember the dialog's parent so closing it returns focus to the
                // window it belonged to, rather than whatever happens to be the
                // workspace's last-focused tile.
                let parent_surface = self.toplevel_parent_surface(&window);
                let should_restore_focus = if ws_idx == self.active_workspace {
                    match self
                        .seat
                        .get_keyboard()
                        .and_then(|keyboard| keyboard.current_focus())
                        .and_then(|target| target.wl_surface().map(|s| s.into_owned()))
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
                        // Prefer the closed dialog's parent if it is still mapped;
                        // fall back to the workspace's last-focused window.
                        let parent_focus = parent_surface
                            .filter(|parent| self.mapped_window_for_surface(parent).is_some());
                        let focus = parent_focus.or_else(|| {
                            self.workspaces[self.active_workspace]
                                .focused_idx
                                .and_then(|focus_idx| {
                                    self.workspaces[self.active_workspace].windows.get(focus_idx)
                                })
                                .and_then(|window| window.toplevel())
                                .map(|toplevel| toplevel.wl_surface().clone())
                        });
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
        // PopupManager::grab_popup wants the keyboard-focus form of the root,
        // not the raw wl_surface. Resolve through `focus_target_for_surface`
        // so popups rooted on an X11 window keep keyboard focus on the X11
        // surface (preserving X11 input focus) instead of falling back to a
        // plain wl_surface.
        let root_target = self.focus_target_for_surface(&root);
        let grab = match self
            .popup_manager
            .grab_popup(root_target, popup, &seat, serial)
        {
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
    type KeyboardFocus = super::focus_target::KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.set_cursor_status(image);
    }

    fn focus_changed(
        &mut self,
        seat: &Seat<Self>,
        focused: Option<&super::focus_target::KeyboardFocusTarget>,
    ) {
        // Smithay hands us the focus target it just routed enter/leave to.
        // Pull the underlying wl_surface for pointer-constraint + selection
        // bookkeeping, which is still wl_surface-keyed.
        let focused_surface = focused
            .and_then(|target| target.wl_surface().map(|s| s.into_owned()));

        // Manage pointer-lock constraints: deactivate on the old focused surface and
        // activate on the newly focused one so games get/lose their pointer lock
        // automatically with keyboard focus.
        if let Some(prev) = self.prev_keyboard_focus.take() {
            if self.deactivate_pointer_constraint_for(&prev) {
                self.set_cursor_status(CursorImageStatus::default_named());
            }
        }
        if let Some(surface) = focused_surface.as_ref() {
            if self.activate_pointer_constraint_for(surface) {
                self.set_cursor_status(CursorImageStatus::Hidden);
            }
        }
        self.prev_keyboard_focus = focused_surface.clone();

        self.note_keyboard_focus_change(focused_surface.as_ref());
        // Deliver the current clipboard/primary selection to the newly focused client.
        let client = focused_surface.as_ref().and_then(|s| s.client());
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

impl XdgDialogHandler for Beewm {
    fn modal_changed(&mut self, toplevel: ToplevelSurface, is_modal: bool) {
        if is_modal {
            self.adopt_floating_dialog_state(&toplevel);
        }
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

impl PointerConstraintsHandler for Beewm {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &smithay::input::pointer::PointerHandle<Self>) {
        // Activate the constraint immediately. Games (e.g. CS2) call lock_pointer and
        // won't start processing WASD/mouse input until they receive the `locked` event.
        // The Wayland spec requires activation when the surface has pointer focus; we
        // satisfy this because games only call lock_pointer when they are focused.
        // Deactivation happens in focus_changed when the surface loses keyboard focus.
        let mut locked_pointer = false;
        with_pointer_constraint(surface, pointer, |constraint| {
            if let Some(c) = constraint {
                locked_pointer = matches!(*c, PointerConstraint::Locked(_));
                if !c.is_active() {
                    c.activate();
                }
            }
        });

        if locked_pointer {
            self.set_cursor_status(CursorImageStatus::Hidden);
        }
    }

    fn cursor_position_hint(
        &mut self,
        _surface: &WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        _location: Point<f64, Logical>,
    ) {
        // Ignored: the cursor stays at its current position when the lock releases.
    }
}

delegate_compositor!(Beewm);
delegate_cursor_shape!(Beewm);
delegate_shm!(Beewm);
delegate_xdg_shell!(Beewm);
delegate_xdg_dialog!(Beewm);
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
delegate_pointer_constraints!(Beewm);
delegate_relative_pointer!(Beewm);

impl FractionalScaleHandler for Beewm {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        self.prime_surface_scale_state(&surface);
    }
}

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
