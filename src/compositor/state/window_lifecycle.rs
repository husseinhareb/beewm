use smithay::desktop::Window;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgToplevelSurfaceData};

use crate::model::window::Geometry;

use super::popup::{
    centered_dialog_position, classify_toplevel_floating, should_map_toplevel_floating,
};
use super::{Beewm, FloatingWindowData, root_surface};

impl Beewm {
    pub(crate) fn centered_floating_data(
        &self,
        window: &Window,
    ) -> Option<(WlSurface, FloatingWindowData)> {
        let root = window.wl_surface().map(|surface| root_surface(&surface))?;
        let output = self.output_for_window(window)?;
        let output_geo = self.space.output_geometry(&output)?;
        // Centre inside the non-exclusive zone so the dialog never slides under
        // beebar or other layer-shell exclusive surfaces.
        let non_exclusive = {
            let lm = smithay::desktop::layer_map_for_output(&output);
            lm.non_exclusive_zone()
        };
        let usable_origin = output_geo.loc + non_exclusive.loc;
        let usable_size = non_exclusive.size;

        let (min_size, max_size) = if let Some(toplevel) = window.toplevel() {
            smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                let mut cached = states
                    .cached_state
                    .get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
                let current = cached.current();
                (current.min_size, current.max_size)
            })
        } else if let Some(x11) = window.x11_surface() {
            // For X11 dialogs (gnome-keyring-prompter, polkit, GTK dialogs)
            // WM_NORMAL_HINTS is the authoritative source of preferred size.
            // The fallback geometry path below works without this but loses a
            // pixel or two whenever the X11 client's natural geometry doesn't
            // match its declared hints.
            (
                x11.min_size().unwrap_or_else(|| Size::from((0, 0))),
                x11.max_size().unwrap_or_else(|| Size::from((0, 0))),
            )
        } else {
            (Size::from((0, 0)), Size::from((0, 0)))
        };
        let bbox_size = window.bbox().size;
        let geo_size = window.geometry().size;

        // Pick the most authoritative size hint we have. The stale tile-size
        // committed before we knew this window was floating tends to match the
        // full usable area on one or both axes — treating bbox/geo == usable as
        // "unreliable" prevents centring a tile-sized rectangle (which collapses
        // to a corner placement). A max_size that meets or exceeds usable is a
        // "no real max" sentinel (32767, display dimensions) and equally
        // unreliable; fall through to bbox/geo just like for an unset cap.
        let pick_axis = |max: i32, bbox: i32, geo: i32, fallback: i32, usable: i32| -> i32 {
            let candidate = if max > 0 && max < usable {
                max
            } else if bbox > 0 && bbox < usable {
                bbox
            } else if geo > 0 && geo < usable {
                geo
            } else {
                fallback
            };
            candidate.min(usable).max(1)
        };

        let default_w = ((usable_size.w / 3).max(320)).min(usable_size.w.max(1));
        let default_h = ((usable_size.h / 3).max(200)).min(usable_size.h.max(1));
        let mut win_w = pick_axis(
            max_size.w,
            bbox_size.w,
            geo_size.w,
            default_w,
            usable_size.w,
        );
        let mut win_h = pick_axis(
            max_size.h,
            bbox_size.h,
            geo_size.h,
            default_h,
            usable_size.h,
        );
        if min_size.w > 0 {
            win_w = win_w.max(min_size.w).min(usable_size.w.max(1));
        }
        if min_size.h > 0 {
            win_h = win_h.max(min_size.h).min(usable_size.h.max(1));
        }

        // Prefer centring over the parent window (transient/modal dialogs sit on
        // top of the window they belong to, like Hyprland/GNOME/KDE); fall back
        // to the usable area when no parent geometry is available. Either way the
        // result is clamped to stay fully on-screen.
        let usable = Rectangle::new(usable_origin, usable_size);
        let win_size = Size::from((win_w, win_h));
        let parent_geo = self.parent_window_geometry(window);
        let pos = centered_dialog_position(usable, parent_geo, win_size);

        tracing::debug!(
            target = "beewm::floating",
            ?usable,
            ?parent_geo,
            ?win_size,
            ?pos,
            centred_over_parent = parent_geo.is_some(),
            "centered_floating_data placement",
        );

        Some((root, FloatingWindowData::new(pos, win_size)))
    }

    /// Geometry of the window this dialog is a child of, if the parent is
    /// currently mapped. Used to centre transient/modal dialogs over their
    /// parent. Only Wayland `xdg_toplevel.set_parent` relationships are resolved
    /// here; cross-client prompters (keyring/polkit) have no parent and fall
    /// back to output-centred placement.
    fn parent_window_geometry(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        let parent = self.toplevel_parent_surface(window)?;
        let parent_window = self.mapped_window_for_surface(&parent)?;
        self.space.element_geometry(&parent_window)
    }

    /// The `xdg_toplevel.set_parent` parent surface of this window, if any.
    pub(crate) fn toplevel_parent_surface(&self, window: &Window) -> Option<WlSurface> {
        let toplevel = window.toplevel()?;
        smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|role| role.lock().unwrap().parent.clone())
        })
    }

    /// Root surface of the currently keyboard-focused window when it is a
    /// floating authentication/modal/transient dialog whose focus a freshly
    /// mapped *tiled* window must not steal.
    ///
    /// "auth dialog" = floating AND (modal | has a parent | known prompter
    /// app_id). This is intentionally narrow: an ordinary floating utility
    /// window does not pin focus forever, but a keyring/polkit/modal prompt
    /// does keep focus when the parent app finishes mapping behind it.
    pub(crate) fn focused_auth_dialog_root(&self) -> Option<WlSurface> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        let surface = focus.wl_surface()?.into_owned();
        let root = root_surface(&surface);
        if !self.is_root_floating(&root) {
            return None;
        }
        let window = self.mapped_window_for_surface(&root)?;
        let class = classify_toplevel_floating(&window);
        (class.is_modal || class.has_parent || class.known_dialog).then_some(root)
    }

    /// Best-effort `app_id`/class for a window, used only for animation log
    /// lines. Returns an empty string when none is available.
    pub(crate) fn window_app_id(window: &Window) -> String {
        window
            .toplevel()
            .and_then(|toplevel| {
                smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                    states
                        .data_map
                        .get::<XdgToplevelSurfaceData>()
                        .and_then(|role| role.lock().unwrap().app_id.clone())
                })
            })
            .or_else(|| window.x11_surface().map(|x11| x11.class()))
            .unwrap_or_default()
    }

    /// The rectangle a window should currently be *rendered* into: the animated
    /// visual rect while an animation is in flight, otherwise its real
    /// `Space` geometry. Used by the renderer and the border generator so the
    /// decorations track the animation. Input/focus never use this — they use
    /// the real geometry from `space`.
    pub(crate) fn visual_geometry(&self, window: &Window) -> Option<Rectangle<i32, Logical>> {
        if let Some(root) = Self::window_root_surface(window) {
            let now = std::time::Instant::now();
            if let Some(visual) = self.animations.active_rect(&root, now) {
                return Some(visual.rect);
            }
        }
        self.space.element_geometry(window)
    }

    /// Advance all window animations by wall-clock time. Returns `true` while
    /// any animation is still active, so the backend keeps scheduling frames at
    /// the output refresh rate; once everything settles it returns `false` and
    /// normal render throttling resumes. Cheap: a couple of `HashMap::retain`s.
    pub fn tick_animations(&mut self, now: std::time::Instant) -> bool {
        let was_active = self.animations.has_active();
        let active = self.animations.tick(now);
        // Repaint while active, and also on the active→idle edge so the final
        // frame settles exactly on the real geometry (the last animated frame
        // is at t<1 and would otherwise stay slightly sub-final on screen).
        if active || was_active {
            self.needs_render = true;
            // Borders are keyed by reused IDs; bump the commit serial so the
            // damage tracker repaints them as the animated rectangle moves.
            self.border_commit_serial = self.border_commit_serial.wrapping_add(1);
        }
        active
    }

    /// Emit the current bottom→top stacking order with each element's app_id
    /// and floating/tiled layer, gated on the `beewm::floating` debug target so
    /// it is free in normal runs.
    pub(crate) fn log_stacking_order(&self, context: &'static str) {
        if !tracing::enabled!(target: "beewm::floating", tracing::Level::DEBUG) {
            return;
        }
        let order: Vec<String> = self
            .space
            .elements()
            .map(|window| {
                let layer = Self::window_root_surface(window)
                    .map(|root| {
                        if self.is_root_floating(&root) {
                            "float"
                        } else {
                            "tiled"
                        }
                    })
                    .unwrap_or("?");
                let app_id = window
                    .toplevel()
                    .and_then(|toplevel| {
                        smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
                            states
                                .data_map
                                .get::<XdgToplevelSurfaceData>()
                                .and_then(|role| role.lock().unwrap().app_id.clone())
                        })
                    })
                    .or_else(|| window.x11_surface().map(|x11| x11.class()))
                    .unwrap_or_default();
                format!("{app_id}[{layer}]")
            })
            .collect();
        tracing::debug!(
            target = "beewm::floating",
            context,
            ?order,
            "stacking order (bottom→top)",
        );
    }

    /// Remove a mapped toplevel (identified by its root wl_surface) from every
    /// structure that tracks it: the owning workspace's window list, the tiling
    /// tree, the floating map, fullscreen state, the surface→window lookup, the
    /// mapped-buffer set, and the space's scene graph. Keyboard focus is moved
    /// to a sensible neighbour and the active workspace is relaid out.
    ///
    /// Shared by the xdg `toplevel_destroyed` path (window gone for good) and
    /// the xdg unmap path (null-buffer commit; the toplevel object may live on
    /// and remap later). Keeping both on one code path guarantees the invariant
    /// that the active tiling tree only ever holds mapped, alive windows.
    ///
    /// Returns the removed window when one was found.
    pub(crate) fn remove_mapped_toplevel(&mut self, target_surface: &WlSurface) -> Option<Window> {
        let root = root_surface(target_surface);

        let Some(ws_idx) = self.workspaces.iter().position(|workspace| {
            workspace
                .windows
                .iter()
                .any(|w| Self::window_root_surface(w).as_ref() == Some(&root))
        }) else {
            // Not in any workspace list. Still scrub any residual bookkeeping so
            // a half-tracked surface can't leave a ghost behind.
            self.mapped_with_buffer.remove(&root);
            self.floating_windows.remove(&root);
            self.sticky_windows.remove(&root);
            self.untrack_window_for_surface(&root);
            return None;
        };

        let pos = self.workspaces[ws_idx]
            .windows
            .iter()
            .position(|w| Self::window_root_surface(w).as_ref() == Some(&root))
            .expect("window presence checked above");
        let window = self.workspaces[ws_idx].remove_window(pos).unwrap();

        // Remember the dialog's parent so closing/unmapping it returns focus to
        // the window it belonged to, rather than whatever happens to be the
        // workspace's last-focused tile.
        let parent_surface = self.toplevel_parent_surface(&window);
        let should_restore_focus = if ws_idx == self.active_workspace() {
            match self
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus())
                .and_then(|target| target.wl_surface().map(|s| s.into_owned()))
            {
                Some(current_focus) => root_surface(&current_focus) == root,
                None => true,
            }
        } else {
            false
        };

        self.untrack_window_for_surface(&root);
        self.mapped_with_buffer.remove(&root);
        // Drop any in-flight animation/target for this window. A true closing
        // shrink animation is not implemented (see `compositor::animation`):
        // it requires a GPU snapshot that outlives the destroyed client buffer.
        // The surviving windows still animate into the freed space via the
        // GeometryChange path triggered by the relayout below.
        self.animations.forget(&root);

        // Clean up fullscreen state if this was the workspace's fullscreen window.
        let was_fullscreen = self.workspaces[ws_idx]
            .fullscreen
            .as_ref()
            .and_then(Self::window_root_surface)
            .map(|fs_root| fs_root == root)
            .unwrap_or(false);
        if was_fullscreen {
            self.workspaces[ws_idx].fullscreen = None;
            // Remap siblings that were unmapped while fullscreen was active.
            for sibling in self.workspaces[ws_idx].windows.clone() {
                if self.space.element_geometry(&sibling).is_none() {
                    self.space.map_element(sibling, (0, 0), false);
                }
            }
        }

        // Clean up floating state and the tiling tree node.
        self.floating_windows.remove(&root);
        self.sticky_windows.remove(&root);
        self.remove_tiled_window(ws_idx, &root);
        self.space.unmap_elem(&window);
        self.publish_workspace_state();

        tracing::debug!(
            target = "beewm::lifecycle",
            id = root.id().protocol_id(),
            ws_idx,
            was_fullscreen,
            remaining_windows = self.workspaces[ws_idx].windows.len(),
            remaining_tiled = self.tiled_windows_in_workspace(ws_idx).len(),
            "removed window from layout",
        );

        if ws_idx == self.active_workspace() {
            if should_restore_focus {
                // Prefer the closed/unmapped dialog's parent if it is still
                // mapped; fall back to the workspace's last-focused window.
                let parent_focus = parent_surface
                    .filter(|parent| self.mapped_window_for_surface(parent).is_some());
                let focus = parent_focus.or_else(|| {
                    self.workspaces[self.active_workspace()]
                        .focused_idx
                        .and_then(|focus_idx| {
                            self.workspaces[self.active_workspace()]
                                .windows
                                .get(focus_idx)
                        })
                        .and_then(Self::window_root_surface)
                });
                self.set_keyboard_focus(focus);
            }
            self.relayout();
            self.needs_render = true;
        }

        Some(window)
    }

    /// Handle an xdg-shell toplevel unmap (the client committed a null buffer
    /// to a previously-mapped toplevel without destroying it). The toplevel
    /// object stays alive, so it leaves the active layout now but is re-queued
    /// as a pending window: if the client remaps it (xdg-shell treats the next
    /// map like an initial map — empty commit, configure, buffer) the normal
    /// first-commit path rebuilds its layout entry from scratch. If the client
    /// instead destroys it, `toplevel_destroyed` drops it from the pending list.
    pub(crate) fn handle_toplevel_unmap(&mut self, target_surface: &WlSurface) {
        let Some(window) = self.remove_mapped_toplevel(target_surface) else {
            return;
        };
        // Only Wayland toplevels reach this path, but guard so a stray X11
        // window (which has its own unmap handler) is never re-queued here.
        if window.toplevel().is_some()
            && !self
                .pending_windows
                .iter()
                .any(|pending| pending == &window)
        {
            self.pending_windows.push(window);
        }
    }

    fn workspace_idx_for_surface(&self, surface: &WlSurface) -> Option<usize> {
        self.workspaces
            .iter()
            .enumerate()
            .find_map(|(workspace_idx, _)| {
                self.window_index_for_surface(workspace_idx, surface)
                    .map(|_| workspace_idx)
            })
    }

    /// The rectangle floating windows are kept reachable within: the output
    /// minus layer-shell exclusive zones (so a float can't hide under the bar),
    /// *without* the tiling gap. Used to clamp interactive moves/resizes so a
    /// floating window can never be dragged entirely off-screen — there are no
    /// client-side titlebars to grab it back with.
    pub(crate) fn floating_usable_rect(&self) -> Option<Rectangle<i32, Logical>> {
        let output = self.focused_output()?;
        self.floating_usable_rect_for(&output)
    }

    /// Floating-window reachable rectangle for a specific output (output minus
    /// its own layer-shell exclusive zones, no tiling gap).
    pub(crate) fn floating_usable_rect_for(
        &self,
        output: &smithay::output::Output,
    ) -> Option<Rectangle<i32, Logical>> {
        let output_geo = self.space.output_geometry(output)?;
        let non_exclusive = {
            let lm = smithay::desktop::layer_map_for_output(output);
            lm.non_exclusive_zone()
        };
        Some(Rectangle::new(
            output_geo.loc + non_exclusive.loc,
            non_exclusive.size,
        ))
    }

    pub(crate) fn tiling_usable_geometry(&self) -> Option<Geometry> {
        let output = self.focused_output()?;
        self.tiling_usable_geometry_for(&output)
    }

    /// Tiling area (output minus that output's exclusive zones, inset by the
    /// configured gap) for a specific output.
    pub(crate) fn tiling_usable_geometry_for(
        &self,
        output: &smithay::output::Output,
    ) -> Option<Geometry> {
        let output_geo = self.space.output_geometry(output)?;
        let gap = self.config.gap as i32;

        let non_exclusive = {
            let lm = smithay::desktop::layer_map_for_output(output);
            lm.non_exclusive_zone()
        };
        let tile_origin = output_geo.loc + non_exclusive.loc;
        let tile_size = non_exclusive.size;

        Some(Geometry::new(
            tile_origin.x + gap,
            tile_origin.y + gap,
            (tile_size.w - gap * 2).max(0) as u32,
            (tile_size.h - gap * 2).max(0) as u32,
        ))
    }

    /// Compute the effective inner gap for a layout cell of the given size.
    ///
    /// When a cell is large enough the returned gap equals the configured gap.
    /// For tiny cells (deep dwindle trees) the gap shrinks so that a 1-pixel
    /// window with its borders never overflows outside the cell boundary.
    fn effective_inner_gap(&self, cell_w: u32, cell_h: u32) -> (i32, i32) {
        let gap = self.config.gap as i32;
        let bw = self.config.border_width as i32;
        let gx = gap.min(((cell_w as i32 - 2 * bw - 1) / 2).max(0));
        let gy = gap.min(((cell_h as i32 - 2 * bw - 1) / 2).max(0));
        (gx, gy)
    }

    pub(crate) fn configured_tiled_size(
        &self,
        geo: Geometry,
    ) -> Size<i32, smithay::utils::Logical> {
        let (gx, gy) = self.effective_inner_gap(geo.width, geo.height);
        let bw = self.config.border_width as i32;
        let w = (geo.width as i32 - gx * 2 - bw * 2).max(1);
        let h = (geo.height as i32 - gy * 2 - bw * 2).max(1);
        Size::from((w, h))
    }

    pub(crate) fn initial_toplevel_size(
        &self,
        surface: &WlSurface,
    ) -> Option<Size<i32, smithay::utils::Logical>> {
        let usable = self.tiling_usable_geometry()?;
        let ws_idx = self.active_workspace();
        let root = root_surface(surface);

        let geo = {
            let split_target = self.focused_tiled_window_root(ws_idx);
            self.layout_manager
                .preview_insert(ws_idx, split_target.as_ref(), root.clone(), &usable)
                .or_else(|| {
                    let tile_count = self.workspaces[ws_idx]
                        .windows
                        .iter()
                        .filter(|window| {
                            let root = Self::window_root_surface(window);
                            let is_fullscreen = root
                                .as_ref()
                                .map(|root| self.is_root_fullscreen(root))
                                .unwrap_or(false);
                            let is_floating = root
                                .as_ref()
                                .map(|root| self.is_root_floating(root))
                                .unwrap_or(false);
                            !is_fullscreen && !is_floating
                        })
                        .count()
                        + 1;
                    self.layout_manager
                        .positional_layout()?
                        .apply(&usable, tile_count)
                        .into_iter()
                        .nth(tile_count - 1)
                })
        }?;

        Some(self.configured_tiled_size(geo))
    }

    /// Toggle the floating state of the currently focused window.
    pub fn toggle_float(&mut self) {
        let window = match self.active_workspace_focused_window().cloned() {
            Some(w) => w,
            None => return,
        };
        let surface = match window.wl_surface().map(|surface| surface.into_owned()) {
            Some(s) => s,
            None => return,
        };
        let root = root_surface(&surface);

        let is_fullscreen = self.is_root_fullscreen(&root);
        let is_floating = self.is_root_floating(&root);

        if is_fullscreen {
            self.exit_fullscreen_internal(false);
        }

        match super::workspace::float_toggle_transition(is_fullscreen, is_floating) {
            super::workspace::FloatToggleTransition::SinkToTiling => {
                self.floating_windows.remove(&root);
                if let Some(toplevel) = window.toplevel() {
                    toplevel.with_pending_state(|s| {
                        s.states.unset(xdg_toplevel::State::Resizing);
                    });
                    toplevel.send_configure();
                }
                let split_target = self.focused_tiled_window_root(self.active_workspace());
                self.insert_tiled_window(self.active_workspace(), &window, split_target.as_ref());
                self.relayout();
            }
            super::workspace::FloatToggleTransition::KeepFloating => {
                self.relayout();
                self.space.raise_element(&window, true);
                self.needs_render = true;
            }
            super::workspace::FloatToggleTransition::MakeFloating => {
                self.float_window(window);
            }
        }
    }

    /// Toggle "show on all workspaces" for the focused window. Marking a tiled
    /// window sticky first floats it — a single surface can't live in every
    /// workspace's tiling tree, so sticky always means floating.
    pub fn toggle_sticky(&mut self) {
        let window = match self.active_workspace_focused_window().cloned() {
            Some(w) => w,
            None => return,
        };
        let Some(root) = Self::window_root_surface(&window) else {
            return;
        };

        if self.sticky_windows.remove(&root) {
            tracing::info!(target: "beewm::floating", id = root.id().protocol_id(), "unstuck window");
            return;
        }

        if !self.is_root_floating(&root) {
            self.float_window(window.clone());
        }
        self.sticky_windows.insert(root.clone());
        self.space.raise_element(&window, true);
        self.needs_render = true;
        tracing::info!(target: "beewm::floating", id = root.id().protocol_id(), "stuck window to all workspaces");
    }

    /// Raise every mapped sticky window to the top of the stack. Called after a
    /// workspace switch so they don't end up behind the new workspace's tiles.
    pub(crate) fn raise_sticky_windows(&mut self) {
        let sticky: Vec<WlSurface> = self.sticky_windows.iter().cloned().collect();
        for root in sticky {
            if let Some(window) = self.mapped_window_for_surface(&root) {
                self.space.raise_element(&window, true);
            }
        }
    }

    /// Swap two tiled windows within a workspace.
    pub fn swap_tiled_windows(
        &mut self,
        workspace_idx: usize,
        first_surface: &WlSurface,
        second_surface: &WlSurface,
    ) -> bool {
        if workspace_idx >= self.workspaces.len() {
            return false;
        }

        let first_root = root_surface(first_surface);
        let second_root = root_surface(second_surface);
        if first_root == second_root
            || self.is_root_floating(&first_root)
            || self.is_root_floating(&second_root)
            || self.is_root_fullscreen(&first_root)
            || self.is_root_fullscreen(&second_root)
        {
            return false;
        }

        if !self
            .layout_manager
            .swap(workspace_idx, &first_root, &second_root)
        {
            return false;
        }

        let Some(first_idx) = self.window_index_for_surface(workspace_idx, &first_root) else {
            return false;
        };
        let Some(second_idx) = self.window_index_for_surface(workspace_idx, &second_root) else {
            return false;
        };

        self.workspaces[workspace_idx].swap_windows(first_idx, second_idx);

        if workspace_idx == self.active_workspace() {
            self.relayout();
        } else {
            self.needs_render = true;
        }

        true
    }

    /// Float a newly-mapped window centered on the screen using its own
    /// natural size.
    ///
    /// For Wayland toplevels we additionally send a `size = None` configure to
    /// release any tile-size hint we may have sent in `new_toplevel` (before we
    /// knew the surface was a dialog). The client will commit again at its
    /// natural size; that commit is intercepted via `pending_float_centers` to
    /// re-centre the window precisely.
    pub fn map_as_floating_centered(&mut self, window: &Window) {
        let Some((root, floating)) = self.centered_floating_data(window) else {
            return;
        };

        self.animations.forget(&root);
        self.floating_windows.insert(root.clone(), floating);
        self.space
            .map_element(window.clone(), floating.position, true);

        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = None;
            });
            toplevel.send_configure();
            self.pending_float_centers.insert(root);
        }
    }

    pub(crate) fn adopt_floating_dialog_state(&mut self, surface: &ToplevelSurface) {
        // The signal that triggered this call (parent_changed / modal_changed)
        // *is* the floating intent — even if the cached XDG role data doesn't
        // yet match `should_map_toplevel_floating` (parent property writes can
        // lag the parent_changed event by a commit). Record the intent so the
        // first-commit path floats the window regardless.
        let is_pending = self.pending_windows.iter().any(|window| {
            window
                .toplevel()
                .map(|toplevel| toplevel.wl_surface() == surface.wl_surface())
                .unwrap_or(false)
        });
        if is_pending {
            self.pending_should_float
                .insert(root_surface(surface.wl_surface()));
            surface.with_pending_state(|state| {
                state.size = None;
            });
            surface.send_configure();
            return;
        }

        let Some(window) = self.mapped_window_for_surface(surface.wl_surface()) else {
            return;
        };
        if !should_map_toplevel_floating(&window) {
            return;
        }

        let Some(root) = window.wl_surface().map(|s| root_surface(&s)) else {
            return;
        };
        let Some(workspace_idx) = self.workspace_idx_for_surface(&root) else {
            return;
        };

        // Ask the client to release its tiled size and commit at its natural
        // (unconstrained) size.  We'll re-center the floating entry once the
        // client responds with that commit.
        surface.with_pending_state(|state| {
            state.size = None;
        });
        surface.send_configure();
        self.pending_float_centers.insert(root.clone());

        let was_floating = self.is_root_floating(&root);
        if !was_floating {
            self.remove_tiled_window(workspace_idx, &root);
        }
        // Insert a placeholder at the output centre; it will be corrected when
        // the client commits at its natural size.
        let placeholder = if let Some((_, floating)) = self.centered_floating_data(&window) {
            floating
        } else {
            return;
        };
        self.animations.forget(&root);
        self.floating_windows.insert(root, placeholder);

        if workspace_idx == self.active_workspace() {
            self.relayout();
            self.space.raise_element(&window, true);
            self.raise_floating_windows();
            if let Some(wl_surface) = window.wl_surface().map(|surface| surface.into_owned()) {
                self.set_keyboard_focus(Some(wl_surface));
            }
        } else {
            self.needs_render = true;
        }
    }

    fn float_window(&mut self, window: Window) {
        let root = match window.wl_surface().map(|surface| root_surface(&surface)) {
            Some(r) => r,
            None => return,
        };
        let output = match self.output_for_window(&window) {
            Some(o) => o,
            None => return,
        };
        let output_geo = self.space.output_geometry(&output).unwrap();
        let float_w = output_geo.size.w / 2;
        let float_h = output_geo.size.h / 2;
        let pos = Point::from((
            output_geo.loc.x + (output_geo.size.w - float_w) / 2,
            output_geo.loc.y + (output_geo.size.h - float_h) / 2,
        ));
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|s| {
                s.states.unset(xdg_toplevel::State::Fullscreen);
                s.size = Some(Size::from((float_w, float_h)));
            });
            toplevel.send_configure();
        }
        self.remove_tiled_window(self.active_workspace(), &root);
        self.animations.forget(&root);
        self.space.map_element(window.clone(), pos, true);
        self.floating_windows.insert(
            root,
            FloatingWindowData::new(pos, Size::from((float_w, float_h))),
        );
        self.relayout();
        self.needs_render = true;
    }

    /// Re-place all floating windows of `ws_idx` back into the space at their
    /// stored positions.
    fn remap_floating_windows_for(&mut self, ws_idx: usize) {
        for window in self.workspaces[ws_idx].windows.clone() {
            let root = match window.wl_surface().map(|surface| surface.into_owned()) {
                Some(surface) => root_surface(&surface),
                None => continue,
            };
            if let Some(floating) = self.floating_windows.get(&root).copied() {
                self.space.map_element(window, floating.position, false);
            }
        }
    }

    /// Toggle fullscreen for the currently focused window.
    pub fn toggle_fullscreen(&mut self) {
        if self.active_fullscreen().is_some() {
            self.exit_fullscreen_internal(true);
        } else {
            let window = match self.active_workspace_focused_window().cloned() {
                Some(w) => w,
                None => return,
            };
            let ws = self.active_workspace();
            self.workspaces[ws].fullscreen = Some(window.clone());
            self.show_fullscreen_window(&window);
        }
    }

    /// Present `window` fullscreen on the active workspace: unmap its siblings,
    /// configure it to the output size, and map it over the output. The active
    /// workspace's `fullscreen` slot must already point at `window` — this only
    /// applies the on-screen presentation. Shared by `toggle_fullscreen` and the
    /// workspace-switch re-apply path so the two can never drift.
    pub(crate) fn show_fullscreen_window(&mut self, window: &Window) {
        let Some(output) = self.output_for_window(window) else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };
        let ws_idx = self.active_workspace();
        for sibling in self.workspaces[ws_idx].windows.clone() {
            if &sibling != window {
                self.space.unmap_elem(&sibling);
            }
        }
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.size = Some(output_geo.size);
            });
            toplevel.send_configure();
        } else if let Some(x11_surface) = window.x11_surface() {
            let _ = x11_surface.set_fullscreen(true);
            let _ = x11_surface.configure(output_geo);
        }
        if let Some(root) = Self::window_root_surface(window) {
            self.animations.forget(&root);
        }
        self.space.map_element(window.clone(), output_geo.loc, true);
        self.needs_render = true;
    }

    /// Exit fullscreen (if any) and restore the tiled layout.
    pub fn restore_fullscreen(&mut self) {
        self.exit_fullscreen_internal(true);
    }

    fn exit_fullscreen_internal(&mut self, relayout: bool) -> Option<Window> {
        let ws = self.active_workspace();
        let fs_window = self.workspaces[ws].fullscreen.take()?;
        let restore_floating = Self::window_root_surface(&fs_window)
            .and_then(|root| self.floating_windows.get(&root).copied());

        if let Some(toplevel) = fs_window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.size = restore_floating.map(|floating| floating.size);
            });
            toplevel.send_configure();
        } else if let Some(x11_surface) = fs_window.x11_surface() {
            let _ = x11_surface.set_fullscreen(false);
            if let Some(floating) = restore_floating {
                let _ = x11_surface.configure(Rectangle::new(floating.position, floating.size));
            }
        }

        if let Some(floating) = restore_floating {
            self.space
                .map_element(fs_window.clone(), floating.position, true);
        }

        let ws_idx = self.active_workspace();
        for window in self.workspaces[ws_idx].windows.clone() {
            if self.space.element_geometry(&window).is_none() {
                self.space.map_element(window, (0, 0), false);
            }
        }

        if relayout {
            self.relayout();
        } else {
            self.needs_render = true;
        }

        Some(fs_window)
    }

    /// Re-tile the visible workspace of every output. Single entry point used
    /// across the compositor; with one output this is the old single-output
    /// relayout.
    pub fn relayout(&mut self) {
        self.relayout_all();
    }

    /// Re-tile the visible workspace on each connected output against that
    /// output's own usable geometry.
    pub fn relayout_all(&mut self) {
        let outputs: Vec<smithay::output::Output> =
            self.outputs.iter().map(|ctx| ctx.output.clone()).collect();
        for output in outputs {
            self.relayout_output(&output);
        }
    }

    /// Re-tile the workspace currently visible on `output`.
    pub(crate) fn relayout_output(&mut self, output: &smithay::output::Output) {
        let Some(ws_idx) = self
            .outputs
            .iter()
            .find(|ctx| &ctx.output == output)
            .map(|ctx| ctx.active_workspace)
        else {
            return;
        };
        let Some(usable) = self.tiling_usable_geometry_for(output) else {
            return;
        };

        let windows = &self.workspaces[ws_idx].windows;
        if windows.is_empty() {
            return;
        }

        let tiled_windows = self.tiled_windows_in_workspace(ws_idx);
        if tiled_windows.is_empty() {
            self.remap_floating_windows_for(ws_idx);
            return;
        }
        let tiled_roots: Vec<WlSurface> = tiled_windows
            .iter()
            .filter_map(Self::window_root_surface)
            .collect();

        let keyed_geos = self
            .layout_manager
            .geometries(ws_idx, &usable, &tiled_roots);

        let now = std::time::Instant::now();
        // Suppress animations entirely while a window owns the whole screen
        // (fullscreen / fullscreen-sized X11 game). beewm has had game FPS and
        // direct-scanout regressions before; animating tiled siblings behind a
        // game is never worth risking that. We still keep targets in sync below
        // so re-tiling later does not snap.
        let suppress_anim =
            self.animations.disable_for_fullscreen() && self.screen_owned_by_window();
        for window in &tiled_windows {
            let Some(root) = Self::window_root_surface(window) else {
                continue;
            };
            let Some(geo) = keyed_geos.get(&root).copied() else {
                continue;
            };
            let (gx, gy) = self.effective_inner_gap(geo.width, geo.height);
            let x = geo.x + gx;
            let y = geo.y + gy;
            let size = self.configured_tiled_size(geo);

            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some(size);
                });
                toplevel.send_pending_configure();
            } else if let Some(x11_surface) = window.x11_surface() {
                let location = Point::from((x, y));
                let _ = x11_surface.configure(smithay::utils::Rectangle::new(location, size));
            }

            let location = Point::from((x, y));
            // Re-map even when the location is unchanged so Smithay refreshes
            // the element placement against the window's current bbox. During
            // interactive tiled resizes, size-only changes otherwise lag a frame
            // behind and exposed regions can briefly show through.
            self.space.map_element(window.clone(), location, false);

            // Visual animation: feed the *final* tile rectangle (location +
            // configured size) to the animation layer. This never changes the
            // logical layout above — it only decides how the window is drawn on
            // the way to this exact rectangle.
            let target = Rectangle::new(location, size);
            if suppress_anim {
                self.animations.track_target(&root, target);
            } else {
                let app_id = Self::window_app_id(window);
                self.animations.reconcile(&root, target, now, &app_id);
            }
        }
        self.needs_render = true;
        self.remap_floating_windows_for(ws_idx);
    }
}
