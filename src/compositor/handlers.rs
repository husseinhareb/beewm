use smithay::delegate_compositor;
use smithay::delegate_cursor_shape;
use smithay::delegate_idle_inhibit;
use smithay::delegate_pointer_constraints;
use smithay::delegate_relative_pointer;
use smithay::wayland::idle_inhibit::IdleInhibitHandler;
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
use smithay::delegate_session_lock;
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
use smithay::desktop::utils::surface_primary_scanout_output;
use smithay::input::keyboard::LedState;
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
    send_surface_state, with_states,
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
use smithay::wayland::session_lock::{
    LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker,
};
use smithay::wayland::shell::xdg::dialog::XdgDialogHandler;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state};
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
    /// Build the per-commit identity/path tags for the `beewm::commit`
    /// diagnostic: which app the surface belongs to, whether it is XWayland,
    /// and — decisively — whether it is the output's primary scan-out surface
    /// (and therefore receives *unthrottled* frame callbacks). See
    /// [`super::diagnostics::SurfaceLabel`].
    fn surface_commit_label(&self, surface: &WlSurface) -> super::diagnostics::SurfaceLabel {
        let window = self.mapped_window_for_surface(surface);
        let (app, is_x11) = match &window {
            Some(w) => (
                super::state::focused_window_title(w),
                w.x11_surface().is_some(),
            ),
            None => (String::new(), false),
        };

        // A surface is on the scan-out path when its recorded primary
        // scan-out output (set during rendering) matches its own output.
        let on_scanout_output = self
            .output_for_surface(surface)
            .map(|output| {
                with_states(surface, |states| {
                    surface_primary_scanout_output(surface, states).as_ref() == Some(&output)
                })
            })
            .unwrap_or(false);

        super::diagnostics::SurfaceLabel {
            app,
            is_x11,
            on_scanout_output,
        }
    }

    fn prime_surface_scale_state(&self, surface: &WlSurface) {
        let Some(output) = self.output_for_surface(surface) else {
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
            // Compute identity/path tags before the &mut borrow of the tracker.
            let label = self.surface_commit_label(surface);
            self.commit_tracker.record(key, cb_latency, buffer, label);
        }

        self.popup_manager.commit(surface);

        // Process buffer attachment for the surface tree — required for
        // the renderer to see committed wl_buffer contents.
        on_commit_buffer_handler::<Self>(surface);

        // Session-lock surfaces aren't windows or layer surfaces; a commit just
        // means the lock UI has new content to present. Schedule a render and
        // stop — none of the window/layer routing below applies to them.
        if self.locked {
            let root = root_surface(surface);
            if self
                .lock_surfaces
                .values()
                .any(|ls| *ls.wl_surface() == root)
            {
                self.needs_render = true;
                return;
            }
        }

        // If this is the initial commit of a pending window, map it now.
        if let Some(pos) = self.pending_windows.iter().position(|w| {
            w.toplevel()
                .map(|t| t.wl_surface() == surface)
                .unwrap_or(false)
        }) {
            let window = self.pending_windows.remove(pos);
            let ws_idx = self.active_workspace();
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
                .map(|s| root_surface(&s))
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

            // Browser Picture-in-Picture (Firefox and Chromium both title it
            // exactly "Picture-in-Picture") follows you across workspaces.
            if class.title.as_deref() == Some("Picture-in-Picture") {
                self.sticky_windows.insert(window_root.clone());
            }

            if should_float {
                self.map_as_floating_centered(&window);
                self.relayout();
            } else {
                self.insert_tiled_window(ws_idx, &window, split_target.as_ref());
                self.relayout();
            }
            let wedge_trace = crate::compositor::runtime_flags::flags().wedge_trace;
            if wedge_trace {
                tracing::warn!(target: "beewm::wedge", "map: after relayout");
            }

            // A fullscreen window owns the keyboard and the whole screen;
            // newly-mapped tiled windows that arrive while it's active must
            // not steal its focus, otherwise the user is left typing into an
            // invisible window underneath and directional focus gets stuck.
            let fullscreen_blocks_focus = self
                .active_fullscreen()
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
                    // Defer focus out of the commit callback: calling
                    // set_keyboard_focus here re-enters this very surface's
                    // cached state (via focus_changed → with_pending_state) and
                    // self-deadlocks the main loop. Applied right after dispatch.
                    self.pending_map_focus = Some(toplevel.wl_surface().clone());
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

            // xdg-shell has no unmap event: a client unmaps a toplevel by
            // committing a null buffer (and may keep the toplevel object alive
            // — e.g. Firefox session restore tears down and rebuilds its
            // window). Detect that here, on the toplevel's own root surface
            // commit, and evict the window from the layout so no stale,
            // invisible node keeps consuming tiling space. Subsurface commits
            // and the no-buffer round-trip commits of the initial map handshake
            // are excluded: we only act on the root surface, and only once the
            // window has actually presented a buffer (`mapped_with_buffer`).
            if let Some(toplevel) = window.toplevel()
                && toplevel.wl_surface() == surface
            {
                let root = root_surface(surface);
                let has_buffer =
                    with_renderer_surface_state(surface, |state| state.buffer().is_some())
                        .unwrap_or(false);
                if has_buffer {
                    self.mapped_with_buffer.insert(root);
                } else if self.mapped_with_buffer.contains(&root) {
                    tracing::debug!(
                        target = "beewm::lifecycle",
                        id = root.id().protocol_id(),
                        "toplevel unmapped (null buffer); evicting from layout",
                    );
                    self.handle_toplevel_unmap(surface);
                    self.needs_render = true;
                    return;
                }
            }

            // If we previously sent a `size = None` configure to release this
            // window from its tiled dimensions, re-center it now that the client
            // has committed at its natural size.
            //
            // Do NOT re-set keyboard focus here: the initial-commit path already
            // focused the window. The natural-size recommit can arrive many
            // milliseconds later, and re-focusing at that point would steal
            // focus from whatever the user clicked on in the meantime.
            let root = root_surface(surface);
            if self.pending_float_centers.remove(&root)
                && let Some((_, floating)) = self.centered_floating_data(&window)
            {
                self.floating_windows.insert(root, floating);
                self.relayout();
                self.space.raise_element(&window, true);
                self.raise_floating_windows();
            }

            self.needs_render = true;
        }

        // Handle layer surface commits: arrange the layer map and, after the
        // configure is sent, grant keyboard focus when the surface requests it.
        let output = self.focused_output();
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
                if let (Some(wl_surface), Some(keyboard)) =
                    (focus_wl_surface, self.seat.get_keyboard())
                {
                    let already_focused = keyboard
                        .current_focus()
                        .and_then(|target| target.wl_surface().map(|s| s.into_owned()))
                        .map(|focused| focused == wl_surface)
                        .unwrap_or(false);
                    if !already_focused {
                        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                        keyboard.set_focus(
                            self,
                            Some(super::focus_target::KeyboardFocusTarget::Wayland(
                                wl_surface,
                            )),
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
        let _guard = super::state::DispatchCallbackGuard::enter();
        // Republish only when the title-changed surface is the focused one —
        // background tabs/clients renaming themselves shouldn't cause the
        // status bar to flicker through unrelated titles.
        let is_focused = self
            .active_workspace_focused_window()
            .and_then(|window| {
                window
                    .toplevel()
                    .map(|toplevel| toplevel.wl_surface().clone())
            })
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

        let ws_idx = self
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, workspace)| workspace.windows.contains(&window).then_some(ws_idx))
            .unwrap_or(self.active_workspace());

        let already_fullscreen = self.workspaces[ws_idx]
            .fullscreen
            .as_ref()
            .map(|fullscreen| fullscreen == &window)
            .unwrap_or(false);
        if already_fullscreen {
            return;
        }
        // Replace any existing fullscreen on the target workspace. When it is the
        // active workspace we restore it properly (remap siblings); for a hidden
        // workspace nothing is on-screen, so just clear the slot.
        if self.workspaces[ws_idx].fullscreen.is_some() {
            if ws_idx == self.active_workspace() {
                self.restore_fullscreen();
            } else {
                self.workspaces[ws_idx].fullscreen = None;
            }
        }

        let Some(output) = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.output_for_window(&window))
        else {
            surface.send_configure();
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            surface.send_configure();
            return;
        };

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
        self.workspaces[ws_idx].fullscreen = Some(window.clone());
        self.set_keyboard_focus(Some(surface.wl_surface().clone()));
        self.needs_render = true;
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let is_current_fullscreen = self
            .active_fullscreen()
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface() == surface.wl_surface())
            .unwrap_or(false);

        if is_current_fullscreen {
            self.restore_fullscreen();
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let target_surface = surface.wl_surface();

        // Remove windows that died before their first commit (still pending) or
        // that were unmapped and re-queued as pending awaiting a remap.
        if let Some(pos) = self.pending_windows.iter().position(|w| {
            w.toplevel()
                .map(|t| t.wl_surface() == target_surface)
                .unwrap_or(false)
        }) {
            self.pending_windows.remove(pos);
            self.pending_should_float.remove(target_surface);
            self.mapped_with_buffer
                .remove(&root_surface(target_surface));
            return;
        }

        // A mapped (or previously-mapped) toplevel: tear it out of every layout
        // structure and relayout. Shared with the unmap path so destroy and
        // unmap can never diverge.
        self.remove_mapped_toplevel(target_surface);
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
        if let Some(pointer) = seat.get_pointer()
            && !pointer.is_grabbed()
        {
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Clear);
        }
        if let Some(keyboard) = seat.get_keyboard()
            && !keyboard.is_grabbed()
        {
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
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
        let _guard = super::state::DispatchCallbackGuard::enter();
        let trace = crate::compositor::runtime_flags::flags().wedge_trace;
        if trace {
            tracing::warn!(target: "beewm::wedge", "focus_changed: begin");
        }
        // Smithay hands us the focus target it just routed enter/leave to.
        // Pull the underlying wl_surface for pointer-constraint + selection
        // bookkeeping, which is still wl_surface-keyed.
        let focused_surface =
            focused.and_then(|target| target.wl_surface().map(|s| s.into_owned()));

        // Manage pointer-lock constraints: deactivate on the old focused surface and
        // activate on the newly focused one so games get/lose their pointer lock
        // automatically with keyboard focus.
        if let Some(prev) = self.prev_keyboard_focus.take()
            && self.deactivate_pointer_constraint_for(&prev)
        {
            self.set_cursor_status(CursorImageStatus::default_named());
        }
        if let Some(surface) = focused_surface.as_ref()
            && self.activate_pointer_constraint_for(surface)
        {
            self.set_cursor_status(CursorImageStatus::Hidden);
        }
        self.prev_keyboard_focus = focused_surface.clone();

        if trace {
            tracing::warn!(target: "beewm::wedge", "focus_changed: constraints done, calling note");
        }
        self.note_keyboard_focus_change(focused_surface.as_ref());
        if trace {
            tracing::warn!(target: "beewm::wedge", "focus_changed: note done, setting selection");
        }
        // Deliver the current clipboard/primary selection to the newly focused client.
        let client = focused_surface.as_ref().and_then(|s| s.client());
        set_data_device_focus::<Self>(&self.display_handle, seat, client.clone());
        set_primary_focus::<Self>(&self.display_handle, seat, client);
        if trace {
            tracing::warn!(target: "beewm::wedge", "focus_changed: done");
        }
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        // Smithay fires this after XKB has processed a key event or a keymap
        // change (config reload) that toggled a lock indicator. Push the new
        // state to physical keyboards through the active backend.
        self.keyboard_leds.apply(led_state.into());
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
            .or_else(|| self.focused_output());

        let output = match output {
            Some(o) => o,
            None => return,
        };

        // Read the client's requested size/anchor before mapping so we can log
        // whether the compositor honors it (a tray menu popup must keep its own
        // content size and never be expanded to the full output).
        let requested = smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            let mut cached = states
                .cached_state
                .get::<smithay::wayland::shell::wlr_layer::LayerSurfaceCachedState>();
            let cur = cached.current();
            (cur.size, cur.anchor, cur.exclusive_zone)
        });

        let desktop_layer = DesktopLayerSurface::new(surface, namespace.clone());
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

        // Diagnostics: prove tray-menu popups keep their requested content size
        // and are not forced to output-sized geometry by the compositor.
        let assigned = layer_map.layer_geometry(&desktop_layer);
        let output_size = output.current_mode().map(|m| m.size).unwrap_or_default();
        tracing::info!(
            "layer surface mapped: namespace={:?} layer={:?} requested_size={:?} \
             anchor={:?} exclusive_zone={:?} assigned_geometry={:?} output_size={:?}",
            namespace,
            _layer,
            requested.0,
            requested.1,
            requested.2,
            assigned,
            output_size,
        );
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
        if let Some(output) = self.focused_output() {
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
        let focus = self.workspaces[self.active_workspace()]
            .focused_idx
            .and_then(|i| self.workspaces[self.active_workspace()].windows.get(i))
            .and_then(|w| w.toplevel())
            .map(|t| t.wl_surface().clone());
        self.set_keyboard_focus(focus);
    }
}

impl SessionLockHandler for Beewm {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.session_lock_manager_state
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        tracing::info!(target: "beewm::lock", "Session lock requested; locking session");
        // Drop normal keyboard focus right away — nothing behind the lock may
        // keep input. Per-output lock surfaces (and their focus) arrive via
        // `new_surface`.
        self.set_keyboard_focus_target(None);
        self.close_overview(false);
        self.overview_hold = None;
        self.locked = true;
        // Confirm to the client that the session is locked. After this the
        // protocol guarantees the session stays locked for this lock's lifetime;
        // we uphold that even across a client crash because `locked` is our own
        // state (see `unlock` / `lock_surface_destroyed`).
        confirmation.lock();
        self.needs_render = true;
    }

    fn unlock(&mut self) {
        tracing::info!(target: "beewm::lock", "Session unlock requested by lock client");
        self.locked = false;
        self.lock_surfaces.clear();
        self.lock_client_last_spawn = None;
        // Hand keyboard focus back to the active window.
        self.focus_current_window();
        self.needs_render = true;
    }

    fn new_surface(&mut self, surface: LockSurface, output: WlOutput) {
        let Some(output) = Output::from_resource(&output) else {
            tracing::warn!(target: "beewm::lock", "Lock surface for an unknown output; ignoring");
            return;
        };

        // Size the lock surface to the full output and send the initial
        // configure so the client can draw. current_mode is in physical pixels;
        // lock surfaces are configured in logical pixels.
        let physical = output.current_mode().map(|m| m.size).unwrap_or_default();
        let logical = physical.to_logical(output.current_scale().integer_scale());
        surface.with_pending_state(|state| {
            state.size = Some((logical.w.max(0) as u32, logical.h.max(0) as u32).into());
        });
        surface.send_configure();

        // Route keyboard focus to the lock surface so the user can type their
        // password. While `locked`, `handle_key` ignores every keybinding, so
        // these keys reach only the lock client.
        let wl_surface = surface.wl_surface().clone();
        self.lock_surfaces.insert(output, surface);
        self.lock_client_last_spawn = None;

        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(
                self,
                Some(super::focus_target::KeyboardFocusTarget::Wayland(
                    wl_surface,
                )),
                serial,
            );
        }
        self.needs_render = true;
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
    fn new_constraint(
        &mut self,
        surface: &WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
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

impl IdleInhibitHandler for Beewm {
    fn inhibit(&mut self, surface: WlSurface) {
        // A client (e.g. a video player) asked us not to idle while this surface
        // is up; remembered so the screen-timeout blank is suppressed.
        self.idle_inhibitors.insert(surface);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.idle_inhibitors.remove(&surface);
    }
}

delegate_compositor!(Beewm);
delegate_cursor_shape!(Beewm);
delegate_idle_inhibit!(Beewm);
delegate_shm!(Beewm);
delegate_xdg_shell!(Beewm);
delegate_xdg_dialog!(Beewm);
delegate_xdg_decoration!(Beewm);
delegate_layer_shell!(Beewm);
delegate_session_lock!(Beewm);
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
