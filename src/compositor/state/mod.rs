mod cursor;
mod decorations;
mod focus;
pub(crate) mod popup;
mod tiling;
mod window_lifecycle;
mod workspace;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::sync::Fence;
use smithay::backend::session::libseat::LibSeatSession;

use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::keyboard::{XkbConfig, xkb};
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::input::{Seat, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{Clock, Logical, Monotonic, Point};
use smithay::wayland::compositor::{
    CompositorClientState, CompositorState, add_blocker, add_pre_commit_hook, get_parent,
    with_states,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::drm_syncobj::{DrmSyncPointSource, DrmSyncobjCachedState, DrmSyncobjState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_constraints::{
    PointerConstraint, PointerConstraintsState, with_pointer_constraint,
};
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::{X11Wm, XWaylandClientData};

use crate::config::{Config, Keybind, LayoutKind};
use crate::layout::manager::{DwindleManager, LayoutManager, MasterStackManager};
use crate::model::workspace::Workspace;
use crate::xwayland::PendingX11Window;

use super::commands::ChildEnvironment;
use super::diagnostics::{CommitTracker, SyncStats};
use super::event_broadcast::EventBroadcaster;
use super::screencopy::{PendingScreencopyFrame, create_screencopy_global};

use super::cursor::CursorThemeManager;

pub use self::decorations::{
    expand_by_border, root_is_swap_highlighted, visible_border_rectangles,
    window_border_overlaps_layer,
};
pub use self::popup::{
    centered_dialog_position, constrain_popup_geometry, is_dialog_size_cap, is_fixed_size,
    popup_constraint_target,
};
pub use self::workspace::{FloatToggleTransition, float_toggle_transition};

const ACTIVE_WORKSPACE_STATE_PATH: &str = "/tmp/beewm_workspace";
const WORKSPACE_STATE_PATH: &str = "/tmp/beewm_workspaces";
const WINDOW_STATE_PATH: &str = "/tmp/beewm_window";
static STATE_FILE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

type SyncobjBlockerInstaller = dyn Fn(DrmSyncPointSource, Client);

pub(crate) use super::types::{ActiveGrab, FloatingWindowData, ResolvedKeybind};

/// The main compositor state.
pub struct Beewm {
    pub running: bool,
    pub config: Config,
    pub start_time: std::time::Instant,
    pub display_handle: DisplayHandle,

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub _xdg_dialog_state: XdgDialogState,
    pub _xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub xwayland_shell_state: XWaylandShellState,
    pub shm_state: ShmState,
    pub _output_manager_state: OutputManagerState,
    pub _viewporter_state: ViewporterState,
    pub _fractional_scale_manager_state: FractionalScaleManagerState,
    pub _single_pixel_buffer_state: SinglePixelBufferState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    pub _screencopy_global: GlobalId,
    pub dmabuf_state: DmabufState,
    pub _dmabuf_global: Option<DmabufGlobal>,
    pub drm_syncobj_state: Option<DrmSyncobjState>,
    pub _presentation_state: PresentationState,
    pub presentation_clock: Clock<Monotonic>,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,

    // Pointer
    pub pointer_location: Point<f64, Logical>,
    pub cursor_status_serial: u64,
    pub cursor_status: CursorImageStatus,
    pub cursor_theme: CursorThemeManager,
    /// Cursor icon override set by the compositor (borders, move grab).
    /// When `Some`, takes priority over the client-requested `cursor_status`.
    pub compositor_cursor_icon: Option<CursorIcon>,
    pub _cursor_shape_manager_state: CursorShapeManagerState,
    /// Keeps the zwp_relative_pointer_manager_v1 global alive so clients can
    /// subscribe to relative mouse motion events (needed by games).
    pub _relative_pointer_state: RelativePointerManagerState,
    /// Protocol state for zwp_pointer_constraints_v1 (pointer lock / confinement).
    pub _pointer_constraints_state: PointerConstraintsState,
    /// The surface that held keyboard focus during the last focus_changed event.
    /// Used to deactivate pointer-lock constraints when focus leaves a surface.
    pub prev_keyboard_focus: Option<WlSurface>,

    // Session (for VT switching in TTY mode)
    pub session: Option<LibSeatSession>,

    // Desktop management
    pub space: Space<Window>,
    pub layout_manager: Box<dyn LayoutManager<WlSurface>>,
    /// Layout state saved while a tiled window is temporarily detached for a drag.
    pub tiled_swap_layout_snapshot: Option<Box<dyn LayoutManager<WlSurface>>>,
    pub workspaces: Vec<Workspace<Window>>,
    pub active_workspace: usize,
    /// Windows that have been created but not yet committed their first buffer.
    pub pending_windows: Vec<Window>,
    /// Root wl_surface -> mapped window lookup for commit-time surface routing.
    pub window_lookup: HashMap<WlSurface, Window>,
    /// Pre-allocated stable IDs for border element fragments.
    /// Reused across frames so the DRM damage tracker sees unchanged geometry.
    pub border_ids: Vec<Id>,
    /// Global commit version for border elements; bumped whenever focus visuals change.
    pub border_commit_serial: u64,
    /// Set when visual state changed and a new frame should be rendered.
    pub needs_render: bool,
    /// X11 window manager state for the compositor-managed XWayland instance.
    pub xwm: Option<X11Wm>,
    /// DISPLAY number exported to spawned child processes once XWayland is ready.
    pub xdisplay: Option<u32>,
    /// The window currently occupying the full screen, if any.
    pub fullscreen_window: Option<Window>,
    /// Tracks popup surfaces and provides grab support.
    pub popup_manager: PopupManager,
    /// Floating windows (not subject to tiling) mapped to their last geometry.
    /// The key is the root WlSurface; the value is where the window is placed
    /// and how large it should be when restored.
    pub floating_windows: HashMap<WlSurface, FloatingWindowData>,
    /// Root WlSurfaces whose window was transitioned from tiled to floating
    /// mid-session. We sent a `size = None` configure so the client will
    /// re-commit at its natural size; on that next commit we re-center the
    /// floating entry to match the client's actual size.
    pub(crate) pending_float_centers: HashSet<WlSurface>,
    /// Toplevel surfaces that announced a floating intent (set_parent, modal,
    /// xdg-dialog) BEFORE their initial commit — at that point the window is
    /// still in `pending_windows` and we can't yet act on the signal. Recorded
    /// here so the first-commit path can honour the intent even if the static
    /// `should_map_toplevel_floating` heuristic doesn't match yet.
    pub(crate) pending_should_float: HashSet<WlSurface>,
    /// Active pointer grab (move, resize, or tiled swap). Only one can be
    /// active at a time.
    pub active_grab: Option<ActiveGrab>,
    /// Current tiled-window swap drop target, if the pointer is over one.
    pub tiled_swap_target: Option<WlSurface>,
    /// Pre-resolved keybindings (no per-keypress string allocs).
    pub resolved_keybinds: Vec<ResolvedKeybind>,
    /// Cached border colours derived from config (avoid per-frame conversion).
    pub border_color_focused: Color32F,
    pub border_color_unfocused: Color32F,
    /// Installs acquire-fence event sources into the active backend loop.
    pub syncobj_blocker_installer: Option<Box<SyncobjBlockerInstaller>>,
    /// Outstanding zwlr_screencopy_frame_v1 objects waiting for a buffer copy.
    pub(crate) pending_screencopy_frames: Vec<PendingScreencopyFrame>,
    /// Compositor-specific environment for spawned child processes.
    pub(crate) child_env: ChildEnvironment,
    /// Pushes `event>>data\n` lines to event-socket subscribers from a
    /// dedicated background thread — the main loop never blocks on socket I/O.
    pub event_broadcaster: EventBroadcaster,
    /// When true, `publish_focused_window_state` will be called once after the
    /// next `event_loop.dispatch()` returns. Setting this from inside dispatch
    /// callbacks (focus_changed, title_changed, X11 property_notify) is safe;
    /// calling `publish_focused_window_state` directly from those callbacks is
    /// NOT, because it re-enters `with_states` on a surface whose cached_state
    /// lock the caller is already holding, deadlocking the entire main loop.
    pub(crate) focus_publish_pending: bool,
    /// Startup commands are delayed until both an output exists and XWayland startup has settled.
    pub(crate) startup_commands_spawned: bool,
    pub(crate) outputs_ready_for_startup: bool,
    pub(crate) xwayland_start_pending: bool,
    pub(crate) pending_x11_windows: Vec<PendingX11Window>,
    /// Per-root-surface commit-rate / responsiveness / buffer-type tracker for
    /// the `beewm::commit` + `beewm::dmabuf` traces. Diagnostic output only.
    pub(crate) commit_tracker: CommitTracker,
    /// Wall-clock time of the most recent `wl_surface.frame` callback batch.
    /// Used to measure how long after we invite a client to draw it actually
    /// commits its next buffer — the key signal for splitting compositor-side
    /// throttling from client-side / GPU-side stalls.
    pub(crate) last_frame_callbacks_sent_at: Option<Instant>,
    /// Explicit-sync acquire-fence activity for the `beewm::sync` trace.
    pub(crate) sync_stats: SyncStats,
    /// X11 windows that sent `_NET_WM_STATE_FULLSCREEN` before they were
    /// mapped into a workspace. Games (especially via Proton) frequently
    /// emit the fullscreen request as part of the same X11 dispatch batch as
    /// `MapWindow`, and our handler ignored requests for unknown windows —
    /// so the game silently stayed windowed. We now record the intent here
    /// and replay it in `map_x11_window` once the window is tracked.
    pub(crate) pending_x11_fullscreen: Vec<smithay::xwayland::X11Surface>,
}

impl Beewm {
    pub fn new(display: &Display<Self>, config: Config) -> Self {
        let display_handle = display.handle();

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_dialog_state = XdgDialogState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<Self>(&display_handle);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let screencopy_global = create_screencopy_global::<Self>(&display_handle);
        let dmabuf_state = DmabufState::new();
        let presentation_clock = Clock::<Monotonic>::new();
        let presentation_state =
            PresentationState::new::<Self>(&display_handle, presentation_clock.id() as u32);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "beewm");

        // Initialize keyboard and pointer on the seat
        seat.add_keyboard(
            XkbConfig {
                layout: &config.keyboard_layout,
                ..Default::default()
            },
            200,
            25,
        )
        .expect("Failed to add keyboard");
        seat.add_pointer();

        let num_ws = config.num_workspaces;
        let layout_manager = build_layout_manager(&config, num_ws);
        let resolved_keybinds = resolve_keybinds(&config.keybinds);
        let border_color_focused = hex_to_color32f(config.border_color_focused);
        let border_color_unfocused = hex_to_color32f(config.border_color_unfocused);
        let cursor_shape_manager_state_ = CursorShapeManagerState::new::<Self>(&display_handle);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);

        let state = Self {
            running: true,
            config,
            start_time: std::time::Instant::now(),
            display_handle: display_handle.clone(),
            compositor_state,
            xdg_shell_state,
            _xdg_dialog_state: xdg_dialog_state,
            _xdg_decoration_state: xdg_decoration_state,
            layer_shell_state,
            xwayland_shell_state,
            shm_state,
            _output_manager_state: output_manager_state,
            _viewporter_state: viewporter_state,
            _fractional_scale_manager_state: fractional_scale_manager_state,
            _single_pixel_buffer_state: single_pixel_buffer_state,
            data_device_state,
            primary_selection_state,
            _screencopy_global: screencopy_global,
            dmabuf_state,
            _dmabuf_global: None,
            drm_syncobj_state: None,
            _presentation_state: presentation_state,
            presentation_clock,
            seat_state,
            seat,
            pointer_location: Point::from((0.0, 0.0)),
            cursor_status_serial: 0,
            cursor_status: CursorImageStatus::default_named(),
            cursor_theme: CursorThemeManager::new(),
            compositor_cursor_icon: None,
            _cursor_shape_manager_state: cursor_shape_manager_state_,
            _relative_pointer_state: relative_pointer_state,
            _pointer_constraints_state: pointer_constraints_state,
            prev_keyboard_focus: None,
            session: None,
            space: Space::default(),
            layout_manager,
            tiled_swap_layout_snapshot: None,
            workspaces: (0..num_ws).map(|_| Workspace::new()).collect(),
            active_workspace: 0,
            pending_windows: Vec::new(),
            window_lookup: HashMap::new(),
            border_ids: Vec::new(),
            border_commit_serial: 0,
            needs_render: true,
            xwm: None,
            xdisplay: None,
            fullscreen_window: None,
            popup_manager: PopupManager::default(),
            floating_windows: HashMap::new(),
            pending_float_centers: HashSet::new(),
            pending_should_float: HashSet::new(),
            active_grab: None,
            tiled_swap_target: None,
            resolved_keybinds,
            border_color_focused,
            border_color_unfocused,
            syncobj_blocker_installer: None,
            pending_screencopy_frames: Vec::new(),
            event_broadcaster: EventBroadcaster::new(),
            focus_publish_pending: false,
            child_env: ChildEnvironment::default(),
            startup_commands_spawned: false,
            outputs_ready_for_startup: false,
            xwayland_start_pending: false,
            pending_x11_windows: Vec::new(),
            commit_tracker: CommitTracker::default(),
            last_frame_callbacks_sent_at: None,
            sync_stats: SyncStats::new(),
            pending_x11_fullscreen: Vec::new(),
        };

        state.publish_workspace_state();
        state.publish_focused_window_state();
        state
    }

    /// Re-read the config file and apply every hot-reloadable field in place.
    /// Called automatically when the config file is saved.
    pub fn apply_config_reload(&mut self) {
        let new_config = match Config::load() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Config reload failed, keeping current config: {}", e);
                return;
            }
        };

        // Border colors are pre-converted to Color32F at init time; keep them in sync.
        if new_config.border_color_focused != self.config.border_color_focused
            || new_config.border_color_unfocused != self.config.border_color_unfocused
            || new_config.border_width != self.config.border_width
        {
            self.border_color_focused = hex_to_color32f(new_config.border_color_focused);
            self.border_color_unfocused = hex_to_color32f(new_config.border_color_unfocused);
            // Bump the commit serial so the DRM damage tracker sees the colour change.
            self.border_commit_serial = self.border_commit_serial.wrapping_add(1);
        }

        // Keybinds are pre-resolved to keysyms; re-resolve on any change.
        if new_config.keybinds != self.config.keybinds {
            self.resolved_keybinds = resolve_keybinds(&new_config.keybinds);
        }

        // Split ratio: propagate to the layout manager.
        // For dwindle this affects future splits; for master-stack it also
        // changes the current master/stack division immediately.
        if new_config.split_ratio != self.config.split_ratio {
            self.layout_manager.set_default_split_ratio(new_config.split_ratio);
        }

        // Layout algorithm change: rebuild the manager from scratch, re-inserting
        // all tiled windows in their current workspace order so nothing is lost.
        if new_config.layout != self.config.layout {
            let num_ws = self.workspaces.len();
            let mut new_manager = build_layout_manager(&new_config, num_ws);
            for ws_idx in 0..num_ws {
                for root in self.tiled_window_roots_in_workspace(ws_idx) {
                    new_manager.insert(ws_idx, None, root);
                }
            }
            self.layout_manager = new_manager;
        }

        // num_workspaces changes require a restart (adding/removing live workspaces
        // while windows exist is not safe to do mid-session).
        if new_config.num_workspaces != self.config.num_workspaces {
            tracing::warn!(
                "num_workspaces changed ({} → {}); restart beewm to apply",
                self.config.num_workspaces,
                new_config.num_workspaces,
            );
        }

        // keyboard_layout: re-apply XKB config so the running session picks it up.
        if new_config.keyboard_layout != self.config.keyboard_layout {
            if let Some(keyboard) = self.seat.get_keyboard() {
                let result = keyboard.set_xkb_config(
                    self,
                    XkbConfig {
                        layout: &new_config.keyboard_layout,
                        ..Default::default()
                    },
                );
                if let Err(e) = result {
                    tracing::warn!("Failed to apply keyboard_layout '{}': {:?}", new_config.keyboard_layout, e);
                }
            }
        }

        // autostart_commands are intentionally not re-executed on reload.
        // tap_to_click / natural_scroll take effect for devices added after
        // this reload; already-connected devices keep their current setting.

        self.config = new_config;
        tracing::info!(
            "Config reloaded: layout={:?}, border_width={}, gap={}, split_ratio={}",
            self.config.layout,
            self.config.border_width,
            self.config.gap,
            self.config.split_ratio,
        );

        // relayout() repositions all windows on the active workspace and sets
        // needs_render = true so the new gap/border values are picked up.
        self.relayout();
    }

    pub fn install_syncobj_blocker_source(&mut self, installer: Box<SyncobjBlockerInstaller>) {
        self.syncobj_blocker_installer = Some(installer);
    }

    pub fn install_explicit_sync_hook(surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _dh, surface| {
            let acquire_point = with_states(surface, |states| {
                let mut cached = states.cached_state.get::<DrmSyncobjCachedState>();
                cached.pending().acquire_point.clone()
            });

            let Some(acquire_point) = acquire_point else {
                return;
            };

            if acquire_point.is_signaled() {
                return;
            }

            let Some(client) = surface.client() else {
                return;
            };

            let Some(installer) = state.syncobj_blocker_installer.as_ref() else {
                return;
            };

            match acquire_point.generate_blocker() {
                Ok((blocker, source)) => {
                    add_blocker(surface, blocker);
                    installer(source, client);
                    // A commit was held waiting on a client GPU fence; the
                    // matching clear is counted in the backend's fence source.
                    state.sync_stats.record_install();
                }
                Err(error) => {
                    tracing::warn!("Failed to install explicit-sync blocker: {}", error);
                }
            }
        });
    }

    pub(crate) fn publish_workspace_state(&self) {
        if super::runtime_flags::flags().workspace_publish_disabled {
            return;
        }
        let active_workspace = active_workspace_state_contents(self.active_workspace);
        if let Err(error) =
            write_state_file_atomically(Path::new(ACTIVE_WORKSPACE_STATE_PATH), &active_workspace)
        {
            tracing::warn!(
                "Failed to publish active workspace to {}: {}",
                ACTIVE_WORKSPACE_STATE_PATH,
                error
            );
        }

        let state = workspace_state_contents(self.active_workspace, &self.workspaces);
        if let Err(error) = write_state_file_atomically(Path::new(WORKSPACE_STATE_PATH), &state) {
            tracing::warn!(
                "Failed to publish workspace state to {}: {}",
                WORKSPACE_STATE_PATH,
                error
            );
        }

        let workspace_num = self.active_workspace + 1;
        self.event_broadcaster.push_event(&format!("workspace>>{workspace_num}"));
    }

    /// Mark the focused-window IPC state as needing a republish. Cheap and
    /// safe to call from any dispatch callback. The actual file write +
    /// event push happens in `flush_pending_focus_publish` after dispatch.
    pub(crate) fn request_focus_publish(&mut self) {
        self.focus_publish_pending = true;
    }

    /// If a republish was requested during the last dispatch, do it now.
    /// MUST be called from the main loop AFTER `event_loop.dispatch()`
    /// returns, never from inside a dispatch callback (see field doc on
    /// `focus_publish_pending`).
    pub(crate) fn flush_pending_focus_publish(&mut self) {
        if !self.focus_publish_pending {
            return;
        }
        self.focus_publish_pending = false;
        self.publish_focused_window_state();
    }

    /// Deactivate any active pointer-lock or confinement constraint on `surface`.
    /// Called when keyboard focus leaves a surface to release games from their lock.
    pub fn deactivate_pointer_constraint_for(&mut self, surface: &WlSurface) -> bool {
        let pointer = match self.seat.get_pointer() {
            Some(p) => p,
            None => return false,
        };

        let mut deactivated_locked = false;
        with_pointer_constraint(surface, &pointer, |constraint| {
            if let Some(c) = constraint {
                let is_locked = matches!(*c, PointerConstraint::Locked(_));
                if c.is_active() {
                    c.deactivate();
                    deactivated_locked = is_locked;
                }
            }
        });
        deactivated_locked
    }

    /// Activate a pending pointer-lock or confinement constraint on `surface`.
    /// Called when keyboard focus arrives at a surface that has a pending constraint.
    pub fn activate_pointer_constraint_for(&mut self, surface: &WlSurface) -> bool {
        let pointer = match self.seat.get_pointer() {
            Some(p) => p,
            None => return false,
        };

        let mut locked_constraint = false;
        with_pointer_constraint(surface, &pointer, |constraint| {
            if let Some(c) = constraint {
                locked_constraint = matches!(*c, PointerConstraint::Locked(_));
                if !c.is_active() {
                    c.activate();
                }
            }
        });
        locked_constraint
    }

    /// Publish the title of the currently-focused window.
    ///
    /// Writes to `/tmp/beewm_window` for compatibility with polling tools and
    /// pushes a `window>>title` event to all connected event-socket subscribers.
    ///
    /// DO NOT call this directly from a Wayland dispatch callback (focus_changed,
    /// title_changed, X11 property_notify, etc) - it calls `with_states` on the
    /// focused toplevel, and those callbacks already hold that surface's
    /// cached_state lock, so re-entering it deadlocks the main loop. Use
    /// `request_focus_publish` from those paths instead.
    pub(crate) fn publish_focused_window_state(&self) {
        if super::runtime_flags::flags().focus_ipc_disabled {
            return;
        }
        let title = self
            .active_workspace_focused_window()
            .map(focused_window_title)
            .unwrap_or_default();
        if let Err(error) = write_state_file_atomically(Path::new(WINDOW_STATE_PATH), &title) {
            tracing::warn!(
                "Failed to publish focused window title to {}: {}",
                WINDOW_STATE_PATH,
                error
            );
        }
        self.event_broadcaster.push_event(&format!("window>>{title}"));
    }
}

/// Resolve the human-readable title of a tracked window, looking on both the
/// xdg-shell and XWayland sides. Returns an empty string when no title is
/// set yet — a not-uncommon state right after window creation.
pub fn focused_window_title(window: &smithay::desktop::Window) -> String {
    if let Some(x11) = window.x11_surface() {
        return x11.title();
    }
    if let Some(toplevel) = window.toplevel() {
        return smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .and_then(|data| data.lock().ok().and_then(|role| role.title.clone()))
                .unwrap_or_default()
        });
    }
    String::new()
}

pub fn active_workspace_state_contents(active_workspace: usize) -> String {
    (active_workspace + 1).to_string()
}

pub fn workspace_state_contents<W>(active_workspace: usize, workspaces: &[Workspace<W>]) -> String {
    let occupied = workspaces
        .iter()
        .enumerate()
        .filter(|(_, workspace)| workspace.window_count() > 0)
        .map(|(index, _)| (index + 1).to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("active={}\noccupied={occupied}\n", active_workspace + 1)
}

pub fn write_state_file_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("beewm_state");
    let temp_path = path.with_file_name(format!(
        ".{file_name}.tmp.{}.{}",
        std::process::id(),
        STATE_FILE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));

    fs::write(&temp_path, contents)?;

    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    Ok(())
}

fn build_layout_manager(
    config: &Config,
    num_workspaces: usize,
) -> Box<dyn LayoutManager<WlSurface>> {
    match config.layout {
        LayoutKind::Dwindle => Box::new(DwindleManager::new(num_workspaces, config.split_ratio)),
        LayoutKind::MasterStack => {
            Box::new(MasterStackManager::new(num_workspaces, config.split_ratio))
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

pub(crate) fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

/// Pre-resolve keybinds so the hot-path is a simple integer comparison.
/// Keycodes are looked up in a base US QWERTY keymap so that bindings fire on
/// physical key position rather than the symbol the active layout produces.
fn resolve_keybinds(keybinds: &[Keybind]) -> Vec<ResolvedKeybind> {
    let us_keycode_map = build_us_keysym_to_keycode_map();

    keybinds
        .iter()
        .map(|bind| {
            let mut logo = false;
            let mut shift = false;
            let mut ctrl = false;
            let mut alt = false;
            for m in &bind.modifiers {
                match m.to_lowercase().as_str() {
                    "super" | "mod4" | "logo" => logo = true,
                    "shift" => shift = true,
                    "ctrl" | "control" => ctrl = true,
                    "alt" | "mod1" => alt = true,
                    _ => {}
                }
            }
            let keysym = xkb::keysym_from_name(&bind.key, xkb::KEYSYM_CASE_INSENSITIVE);
            let keycode = us_keycode_map.get(&keysym).copied();
            ResolvedKeybind {
                logo,
                shift,
                ctrl,
                alt,
                keycode,
                keysym,
                action: bind.action.clone(),
            }
        })
        .collect()
}

/// Build a map from keysym → XKB keycode using a base US QWERTY layout so
/// that keybind resolution is position-based instead of symbol-based.
fn build_us_keysym_to_keycode_map() -> std::collections::HashMap<xkb::Keysym, xkb::Keycode> {
    let mut map = std::collections::HashMap::new();
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let Some(keymap) = xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        "us",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) else {
        tracing::warn!("Failed to build US keymap for position-based keybind resolution");
        return map;
    };

    let min = keymap.min_keycode();
    let max = keymap.max_keycode();
    let mut code = min;
    loop {
        let syms = keymap.key_get_syms_by_level(code, 0, 0);
        if let Some(&sym) = syms.first() {
            map.entry(sym).or_insert(code);
        }
        if code == max {
            break;
        }
        code = xkb::Keycode::new(code.raw() + 1);
    }
    map
}

pub(crate) fn lookup_client_compositor_state(client: &Client) -> Option<&CompositorClientState> {
    client
        .get_data::<ClientState>()
        .map(|state| &state.compositor_state)
        .or_else(|| {
            client
                .get_data::<XWaylandClientData>()
                .map(|state| &state.compositor_state)
        })
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
