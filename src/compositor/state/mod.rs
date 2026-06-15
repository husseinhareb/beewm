mod cursor;
mod decorations;
mod focus;
mod output;
pub(crate) mod popup;
mod tiling;
mod window_lifecycle;
mod workspace;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::sync::Fence;
use smithay::backend::session::libseat::LibSeatSession;

use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::keyboard::{XkbConfig, xkb};
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::channel::Sender;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::IsAlive;
use smithay::utils::{Clock, Logical, Monotonic, Point, Size};
use smithay::wayland::compositor::{
    CompositorClientState, CompositorState, add_blocker, add_pre_commit_hook, get_parent,
    with_states,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::drm_syncobj::{DrmSyncPointSource, DrmSyncobjCachedState, DrmSyncobjState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_constraints::{
    PointerConstraint, PointerConstraintsState, with_pointer_constraint,
};
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::session_lock::{LockSurface, SessionLockManagerState};
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::dialog::XdgDialogState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::xwayland_shell::XWaylandShellState;
use smithay::xwayland::{X11Wm, XWaylandClientData};

use crate::compositor::animation::AnimationManager;
use crate::config::{Config, Keybind, LayoutKind, OutputModeSpec};
use crate::layout::manager::{DwindleManager, LayoutManager, MasterStackManager};
use crate::model::workspace::Workspace;
use crate::xwayland::PendingX11Window;

use super::commands::{ChildEnvironment, spawn_shell_command};
use super::diagnostics::{CommitTracker, SyncStats};
use super::event_broadcast::EventBroadcaster;
use super::input::leds::{KeyboardLeds, KeyboardStatus};
use super::screencopy::{PendingScreencopyFrame, create_screencopy_global};

use super::cursor::CursorThemeManager;
use super::tray::{MenuAction, MenuItem, ModeMenu, SharedMenu, TrayHandle, build_menu_items};

pub use self::decorations::{
    expand_by_border, root_is_swap_highlighted, visible_border_rectangles,
    window_border_overlaps_layer,
};
pub use self::output::OutputCtx;
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

/// Work the compositor wants the backend to perform on its next loop turn,
/// because it needs DRM/GPU access the compositor side does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendRequest {
    /// Power the outputs off (`on = false`, via `DrmCompositor::clear`) or back
    /// on (`on = true`, by resuming rendering). Drives screen-timeout blanking.
    SetDpms { on: bool },
    /// Switch `output` to `mode` (a live DRM modeset). Gated on
    /// `BEEWM_LIVE_MODESET`; logged and skipped otherwise.
    SetOutputMode {
        output: Output,
        mode: OutputModeSpec,
    },
}

/// The mode list reported by the backend for one output, used to populate the
/// tray's Resolution/Refresh submenus.
#[derive(Debug, Clone, Default)]
pub struct OutputModes {
    /// Every mode the connector advertises (`WxH@Hz`).
    pub available: Vec<OutputModeSpec>,
    /// The mode currently driving the output.
    pub current: Option<OutputModeSpec>,
}

/// The main compositor state.
pub struct Beewm {
    pub running: bool,
    pub config: Config,
    /// Whether to publish the settings tray icon (a StatusNotifierItem on the
    /// session bus). Resolved from config/env at startup.
    pub tray_enabled: bool,
    /// The current settings-menu tree, shared with the D-Bus tray thread which
    /// reads it whenever the host opens the menu. Rebuilt by `refresh_tray_menu`.
    pub tray_menu: SharedMenu,
    /// Sender side of the calloop tray-action channel, installed once by the
    /// active backend so config reloads can start the tray after startup.
    pub tray_action_tx: Option<Sender<MenuAction>>,
    /// Running StatusNotifierItem thread, if the settings tray is enabled.
    pub tray_handle: Option<TrayHandle>,
    /// Last time any input was seen; drives screen-timeout blanking.
    pub last_activity: Instant,
    /// True while the outputs are blanked (DPMS off) by the idle timeout.
    pub blanked: bool,
    /// Work queued for the backend to apply (DPMS now; output modes later).
    pub backend_requests: VecDeque<BackendRequest>,
    /// `zwp_idle_inhibit` manager global.
    pub idle_inhibitor_state: IdleInhibitManagerState,
    /// Surfaces currently inhibiting idle (e.g. a video player). While any is
    /// alive, the screen-timeout blank is suppressed.
    pub idle_inhibitors: HashSet<WlSurface>,
    /// Mode list per output (backend-reported), for the tray's Resolution menu.
    pub output_modes: HashMap<Output, OutputModes>,
    /// Output modes written by the tray to `state.conf`, keyed by output name.
    pub runtime_output_modes: HashMap<String, OutputModeSpec>,
    pub start_time: std::time::Instant,
    pub display_handle: DisplayHandle,

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub _xdg_dialog_state: XdgDialogState,
    pub _xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    /// ext-session-lock-v1: lets a locker (beelock) take a real,
    /// compositor-enforced lock. This is the *only* secure lock path — see
    /// the `locked` field below.
    pub session_lock_manager_state: SessionLockManagerState,
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
    /// Lock-key LED plumbing: the backend controller (installed by the udev
    /// backend, libinput-backed) plus the last-applied cache. The nested
    /// winit backend installs no controller — the host compositor owns the
    /// keyboards — so LED writes are a safe no-op there.
    pub keyboard_leds: KeyboardLeds,
    /// Last `keyboard>>…` lock/Shift snapshot pushed to the event socket,
    /// so subscribers only hear about actual changes.
    pub(crate) last_keyboard_status: Option<KeyboardStatus>,

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

    // ── Session lock (ext-session-lock-v1) ──────────────────────────────────
    /// True while the session is locked by an ext-session-lock client.
    ///
    /// This is the load-bearing security flag: when set, the compositor renders
    /// only lock surfaces (or solid black for any output without one), routes
    /// all keyboard/pointer input exclusively to the lock surface, and ignores
    /// every compositor keybinding. Crucially it lives entirely in compositor
    /// state, so if the lock client dies the session STAYS locked (black screen,
    /// input still trapped) until a new locker authenticates — a killed process
    /// can never expose the session.
    pub locked: bool,
    /// Per-output lock surfaces supplied by the lock client. Absence of a
    /// surface for a locked output means that output renders solid black.
    pub lock_surfaces: HashMap<Output, LockSurface>,
    /// Last attempt to spawn the configured lock client. Used to avoid a tight
    /// respawn loop if the command is missing or exits immediately.
    pub(crate) lock_client_last_spawn: Option<Instant>,

    // Desktop management
    pub space: Space<Window>,
    pub layout_manager: Box<dyn LayoutManager<WlSurface>>,
    /// Layout state saved while a tiled window is temporarily detached for a drag.
    pub tiled_swap_layout_snapshot: Option<Box<dyn LayoutManager<WlSurface>>>,
    pub workspaces: Vec<Workspace<Window>>,
    /// Registry of outputs known to the compositor, kept in sync with the
    /// `Space` via [`Beewm::add_output`]. The single source of truth for which
    /// outputs exist and where they are positioned.
    pub outputs: Vec<OutputCtx>,
    /// Index into `outputs` that currently owns keyboard focus and receives
    /// newly-mapped windows. Always valid while `outputs` is non-empty.
    pub focused_output: usize,
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
    /// Whether beewm owns the real session and may push its environment
    /// (WAYLAND_DISPLAY, XDG_CURRENT_DESKTOP, …) into the D-Bus/systemd
    /// activation environment so bus-activated portals/PipeWire find the
    /// display. Only the udev (TTY) backend sets this; the nested winit backend
    /// leaves it false so it never clobbers the host session's portal env.
    pub session_env_managed: bool,
    /// Compositor-driven window animations (open / layout-resize). Purely
    /// visual: the logical layout in `space`/`layout_manager` is always exact.
    pub animations: AnimationManager,
    /// X11 window manager state for the compositor-managed XWayland instance.
    pub xwm: Option<X11Wm>,
    /// DISPLAY number exported to spawned child processes once XWayland is ready.
    pub xdisplay: Option<u32>,
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
    /// Root WlSurfaces of mapped toplevels that have presented a buffer at
    /// least once. xdg-shell has no dedicated unmap event: a client unmaps a
    /// toplevel by committing a null buffer (and may keep the toplevel object
    /// alive — e.g. Firefox session restore recreating its window). We treat a
    /// null-buffer commit from a surface in this set as an unmap and evict the
    /// window from the tiling layout so no stale node keeps consuming space.
    /// Only roots that have actually shown a buffer are eligible, so the
    /// no-buffer round-trip commits during the initial map handshake never
    /// trip the unmap path.
    pub(crate) mapped_with_buffer: HashSet<WlSurface>,
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
    /// Keyboard focus to assign to a freshly-mapped window, deferred out of the
    /// `commit()` callback. Calling `set_keyboard_focus` synchronously during a
    /// surface's own commit re-enters `with_pending_state`/`send_pending_configure`
    /// on that same surface and self-deadlocks the main loop (same hazard as
    /// `focus_publish_pending`). Applied right after dispatch instead.
    pub(crate) pending_map_focus: Option<WlSurface>,
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
        // Allow any client to bind the session-lock manager. Only one lock can
        // be held at a time; the protocol arbitrates that for us.
        let session_lock_manager_state =
            SessionLockManagerState::new::<Self, _>(&display_handle, |_| true);
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
        // The tray icon is published when the config opts in OR the BEEWM_TRAY
        // env flag is set.
        let tray_enabled = Self::resolve_tray_enabled(&config);
        let tray_menu: SharedMenu = Arc::new(Mutex::new(Vec::new()));
        let runtime_output_modes = Config::runtime_output_modes().into_iter().collect();
        let idle_inhibitor_state = IdleInhibitManagerState::new::<Self>(&display_handle);
        let animations = AnimationManager::from_config(&config);
        let cursor_shape_manager_state_ = CursorShapeManagerState::new::<Self>(&display_handle);
        let relative_pointer_state = RelativePointerManagerState::new::<Self>(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<Self>(&display_handle);

        let state = Self {
            running: true,
            config,
            tray_enabled,
            tray_menu,
            tray_action_tx: None,
            tray_handle: None,
            last_activity: std::time::Instant::now(),
            blanked: false,
            backend_requests: VecDeque::new(),
            idle_inhibitor_state,
            idle_inhibitors: HashSet::new(),
            output_modes: HashMap::new(),
            runtime_output_modes,
            start_time: std::time::Instant::now(),
            display_handle: display_handle.clone(),
            compositor_state,
            xdg_shell_state,
            _xdg_dialog_state: xdg_dialog_state,
            _xdg_decoration_state: xdg_decoration_state,
            layer_shell_state,
            session_lock_manager_state,
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
            keyboard_leds: KeyboardLeds::new(),
            last_keyboard_status: None,
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
            locked: false,
            lock_surfaces: HashMap::new(),
            lock_client_last_spawn: None,
            space: Space::default(),
            layout_manager,
            tiled_swap_layout_snapshot: None,
            workspaces: (0..num_ws).map(|_| Workspace::new()).collect(),
            outputs: Vec::new(),
            focused_output: 0,
            pending_windows: Vec::new(),
            window_lookup: HashMap::new(),
            border_ids: Vec::new(),
            border_commit_serial: 0,
            needs_render: true,
            session_env_managed: false,
            animations,
            xwm: None,
            xdisplay: None,
            popup_manager: PopupManager::default(),
            floating_windows: HashMap::new(),
            pending_float_centers: HashSet::new(),
            pending_should_float: HashSet::new(),
            mapped_with_buffer: HashSet::new(),
            active_grab: None,
            tiled_swap_target: None,
            resolved_keybinds,
            border_color_focused,
            border_color_unfocused,
            syncobj_blocker_installer: None,
            pending_screencopy_frames: Vec::new(),
            event_broadcaster: EventBroadcaster::new(),
            focus_publish_pending: false,
            pending_map_focus: None,
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

    fn resolve_tray_enabled(config: &Config) -> bool {
        config.tray_enabled || crate::compositor::runtime_flags::flags().tray_enabled
    }

    pub(crate) fn install_tray_action_sender(&mut self, tray_action_tx: Sender<MenuAction>) {
        self.tray_action_tx = Some(tray_action_tx);
        self.sync_tray();
    }

    pub(crate) fn sync_tray(&mut self) {
        if self
            .tray_handle
            .as_ref()
            .map(|handle| handle.is_finished())
            .unwrap_or(false)
        {
            self.tray_handle.take();
        }

        if self.tray_enabled {
            if self.tray_handle.is_some() {
                return;
            }

            let Some(tray_action_tx) = self.tray_action_tx.clone() else {
                tracing::warn!(
                    target: "beewm::tray",
                    "settings tray is enabled but the backend has not installed an action channel",
                );
                return;
            };

            self.refresh_tray_menu();
            self.tray_handle =
                crate::compositor::tray::spawn(self.tray_menu.clone(), tray_action_tx);
        } else if let Some(tray_handle) = self.tray_handle.take() {
            tray_handle.shutdown();
        }
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
            self.layout_manager
                .set_default_split_ratio(new_config.split_ratio);
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
        if new_config.keyboard_layout != self.config.keyboard_layout
            && let Some(keyboard) = self.seat.get_keyboard()
        {
            let result = keyboard.set_xkb_config(
                self,
                XkbConfig {
                    layout: &new_config.keyboard_layout,
                    ..Default::default()
                },
            );
            if let Err(e) = result {
                tracing::warn!(
                    "Failed to apply keyboard_layout '{}': {:?}",
                    new_config.keyboard_layout,
                    e
                );
            }
        }

        // Settings tray: re-resolve enabled (config OR BEEWM_TRAY), then
        // `sync_tray` below starts/stops the StatusNotifierItem thread.
        self.tray_enabled = Self::resolve_tray_enabled(&new_config);

        // autostart_commands are intentionally not re-executed on reload.
        // tap_to_click / natural_scroll take effect for devices added after
        // this reload; already-connected devices keep their current setting.

        self.config = new_config;
        self.animations.update_from_config(&self.config);
        self.refresh_tray_menu();
        self.sync_tray();
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

    /// Rebuild the shared settings-menu tree from current state and publish it
    /// to the D-Bus tray thread (which reads it the next time the host opens the
    /// menu). Cheap; call it whenever an input to `build_tray_menu` changes.
    pub(crate) fn refresh_tray_menu(&self) {
        if let Ok(mut menu) = self.tray_menu.lock() {
            *menu = self.build_tray_menu();
        }
    }

    /// Record the backend-reported mode list for `output` (drives the tray's
    /// Resolution/Refresh submenus) and refresh the published menu.
    pub(crate) fn set_output_modes(&mut self, output: Output, modes: OutputModes) {
        self.output_modes.insert(output, modes);
        self.refresh_tray_menu();
    }

    /// Build the tray menu, populating Resolution/Refresh from the anchor
    /// output's live mode list when known.
    fn build_tray_menu(&self) -> Vec<MenuItem> {
        let mode_menu = self
            .focused_output()
            .and_then(|output| self.output_modes.get(&output))
            .map(|modes| {
                let current_res = modes.current.map(|m| (m.width, m.height));
                let mut resolutions: Vec<(i32, i32)> = modes
                    .available
                    .iter()
                    .map(|m| (m.width, m.height))
                    .collect();
                resolutions.sort_unstable_by(|a, b| b.cmp(a));
                resolutions.dedup();
                let mut refresh_rates: Vec<u32> = modes
                    .available
                    .iter()
                    .filter(|m| current_res == Some((m.width, m.height)))
                    .filter_map(|m| m.refresh)
                    .collect();
                refresh_rates.sort_unstable_by(|a, b| b.cmp(a));
                refresh_rates.dedup();
                ModeMenu {
                    resolutions,
                    refresh_rates,
                    current_res,
                    current_hz: modes.current.and_then(|m| m.refresh),
                }
            });
        build_menu_items(
            mode_menu.as_ref(),
            self.config.gap,
            self.config.screen_timeout,
        )
    }

    /// Persist tray-settable values so they survive a restart (written to the
    /// `state.conf` overlay, not the user's hand-edited config).
    fn persist_runtime_settings(&self) {
        let output_modes: Vec<_> = self
            .runtime_output_modes
            .iter()
            .map(|(name, mode)| (name.clone(), *mode))
            .collect();
        Config::write_state_overrides(self.config.gap, self.config.screen_timeout, &output_modes);
    }

    /// Remember a successfully applied tray output-mode change and persist it.
    pub(crate) fn record_runtime_output_mode(&mut self, output_name: String, mode: OutputModeSpec) {
        self.runtime_output_modes.insert(output_name.clone(), mode);
        self.config.set_output_mode_override(output_name, mode);
        self.persist_runtime_settings();
    }

    /// Apply a menu action chosen in the tray. Called from the D-Bus tray
    /// channel on the main loop.
    pub(crate) fn apply_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::SetGap(gap) => {
                tracing::info!(target: "beewm::tray", gap, "set gap from tray");
                self.config.gap = gap;
                self.relayout();
                self.persist_runtime_settings();
                self.refresh_tray_menu();
            }
            MenuAction::SignOut => {
                tracing::info!(target: "beewm::tray", "sign out from tray");
                self.running = false;
            }
            MenuAction::SetScreenTimeout(secs) => {
                tracing::info!(target: "beewm::tray", secs, "set screen timeout from tray");
                self.config.screen_timeout = secs;
                // Restart the countdown from now (and wake if currently blanked).
                self.notify_activity();
                self.persist_runtime_settings();
                self.refresh_tray_menu();
            }
            MenuAction::SetResolution { width, height } => {
                if let Some(output) = self.focused_output() {
                    tracing::info!(target: "beewm::tray", width, height, "set resolution from tray");
                    self.backend_requests
                        .push_back(BackendRequest::SetOutputMode {
                            output,
                            mode: OutputModeSpec {
                                width,
                                height,
                                // Pick the best refresh the backend can match at this size.
                                refresh: None,
                            },
                        });
                }
            }
            MenuAction::SetRefresh { hz } => {
                if let Some(output) = self.focused_output() {
                    // Keep the current resolution, change only the refresh rate.
                    if let Some((width, height)) = self
                        .output_modes
                        .get(&output)
                        .and_then(|m| m.current)
                        .map(|m| (m.width, m.height))
                    {
                        tracing::info!(target: "beewm::tray", hz, "set refresh from tray");
                        self.backend_requests
                            .push_back(BackendRequest::SetOutputMode {
                                output,
                                mode: OutputModeSpec {
                                    width,
                                    height,
                                    refresh: Some(hz),
                                },
                            });
                    }
                }
            }
        }
    }

    /// Record user input: restart the screen-timeout countdown and, if the
    /// screen was blanked, wake it back up. Called from the input handlers.
    pub(crate) fn notify_activity(&mut self) {
        self.last_activity = Instant::now();
        if self.blanked {
            self.blanked = false;
            self.backend_requests
                .push_back(BackendRequest::SetDpms { on: true });
            self.needs_render = true;
        }
    }

    /// Enter the compositor-enforced secure lock state and start the configured
    /// session-lock client if needed.
    ///
    /// This does not wait for the lock client to draw. Until a real lock
    /// surface exists the renderer shows solid black, and input is prevented
    /// from reaching normal clients by `locked`.
    pub(crate) fn secure_lock(&mut self, reason: &'static str) {
        self.cancel_interactions_for_lock();
        self.pending_map_focus = None;
        self.set_keyboard_focus_target(None);

        if !self.locked {
            tracing::info!(target: "beewm::lock", reason, "secure lock engaged");
            self.locked = true;
        } else {
            tracing::debug!(target: "beewm::lock", reason, "secure lock already engaged");
        }

        self.ensure_lock_client_running(reason);
        self.needs_render = true;
    }

    /// Reassert the lock after resume and wake outputs if the idle timeout had
    /// powered them down before suspend.
    pub(crate) fn secure_resume_lock(&mut self, force_lock: bool) {
        if force_lock || self.locked {
            self.secure_lock("resume");
        }
        self.notify_activity();
        self.needs_render = true;
    }

    fn ensure_lock_client_running(&mut self, reason: &'static str) {
        if !self.locked || !self.lock_surfaces.is_empty() {
            return;
        }

        let command = self.config.lock_command.trim();
        if command.is_empty() {
            tracing::warn!(
                target: "beewm::lock",
                reason,
                "lock_command is empty; staying on compositor black lock screen",
            );
            return;
        }

        let now = Instant::now();
        if self
            .lock_client_last_spawn
            .map(|last| now.saturating_duration_since(last) < Duration::from_secs(2))
            .unwrap_or(false)
        {
            return;
        }
        self.lock_client_last_spawn = Some(now);

        tracing::info!(
            target: "beewm::lock",
            reason,
            command,
            "spawning session lock client",
        );
        if let Err(error) = spawn_shell_command(command, &self.child_env) {
            tracing::error!(
                target: "beewm::lock",
                %error,
                command,
                "failed to spawn session lock client; session remains compositor-locked",
            );
        }
    }

    fn cancel_interactions_for_lock(&mut self) {
        let active_grab = self.active_grab.take();
        if let Some(super::types::ActiveGrab::TiledSwap(grab)) = &active_grab {
            if let Some(layout_snapshot) = self.tiled_swap_layout_snapshot.take() {
                self.layout_manager = layout_snapshot;
            }
            if let Some(root) = Self::window_root_surface(&grab.window) {
                self.floating_windows.remove(&root);
            }
            self.tiled_swap_target = None;
            self.relayout();
        } else {
            self.tiled_swap_layout_snapshot = None;
            self.tiled_swap_target = None;
        }

        match active_grab {
            Some(super::types::ActiveGrab::Resize(grab)) => {
                if let Some(toplevel) = grab.window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.states.unset(xdg_toplevel::State::Resizing);
                        state.size = Some(Size::from((
                            grab.current_window_size.w,
                            grab.current_window_size.h,
                        )));
                    });
                    toplevel.send_configure();
                }
            }
            Some(super::types::ActiveGrab::TiledResize(grab)) => {
                if let Some(toplevel) = grab.window.toplevel() {
                    toplevel.with_pending_state(|state| {
                        state.states.unset(xdg_toplevel::State::Resizing);
                        state.size = None;
                    });
                    toplevel.send_configure();
                }
            }
            Some(super::types::ActiveGrab::Move(_))
            | Some(super::types::ActiveGrab::TiledSwap(_))
            | None => {}
        }

        if let Some(focused) = self.prev_keyboard_focus.clone() {
            self.deactivate_pointer_constraint_for(&focused);
        }
        if let Some(focused) = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|target| target.wl_surface().map(|surface| surface.into_owned()))
        {
            self.deactivate_pointer_constraint_for(&focused);
        }

        self.compositor_cursor_icon = None;
        self.refresh_compositor_cursor();
    }

    /// Any live idle inhibitor (e.g. a video player) suppresses blanking.
    fn idle_inhibited(&mut self) -> bool {
        self.idle_inhibitors.retain(|s| s.alive());
        !self.idle_inhibitors.is_empty()
    }

    /// Evaluate the screen-timeout deadline; blank (DPMS off) when reached.
    /// Called once per backend loop iteration.
    pub(crate) fn update_idle(&mut self, now: Instant) {
        let idle = now.saturating_duration_since(self.last_activity);
        if should_blank(
            self.config.screen_timeout,
            self.blanked,
            self.idle_inhibited(),
            idle,
        ) {
            tracing::info!(target: "beewm::idle", "screen timeout reached; blanking (DPMS off)");
            self.blanked = true;
            self.backend_requests
                .push_back(BackendRequest::SetDpms { on: false });
        }
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
        let active_workspace = active_workspace_state_contents(self.active_workspace());
        if let Err(error) =
            write_state_file_atomically(Path::new(ACTIVE_WORKSPACE_STATE_PATH), &active_workspace)
        {
            tracing::warn!(
                "Failed to publish active workspace to {}: {}",
                ACTIVE_WORKSPACE_STATE_PATH,
                error
            );
        }

        let state = workspace_state_contents(self.active_workspace(), &self.workspaces);
        if let Err(error) = write_state_file_atomically(Path::new(WORKSPACE_STATE_PATH), &state) {
            tracing::warn!(
                "Failed to publish workspace state to {}: {}",
                WORKSPACE_STATE_PATH,
                error
            );
        }

        let workspace_num = self.active_workspace() + 1;
        self.event_broadcaster
            .push_event(&format!("workspace>>{workspace_num}"));
    }

    /// Push the seat keyboard's current XKB LED state to physical keyboards.
    /// Cheap when nothing changed (the write is skipped). Used wherever the
    /// LED state may be stale outside the normal `led_state_changed` flow,
    /// e.g. after a VT switch back (paired with `keyboard_leds.invalidate()`).
    pub fn sync_keyboard_leds(&mut self) {
        if let Some(keyboard) = self.seat.get_keyboard() {
            self.keyboard_leds.apply(keyboard.led_state().into());
        }
    }

    /// Snapshot of the lock/Shift state on the seat keyboard, for the
    /// `keyboard>>…` event-socket line. Shift is reported here (it has no
    /// physical LED); Scroll Lock comes from the XKB indicator since it is
    /// not a modifier.
    pub fn keyboard_status(&self) -> Option<KeyboardStatus> {
        let keyboard = self.seat.get_keyboard()?;
        let modifiers = keyboard.modifier_state();
        let leds = keyboard.led_state();
        Some(KeyboardStatus {
            caps_lock: modifiers.caps_lock,
            num_lock: modifiers.num_lock,
            scroll_lock: leds.scroll.unwrap_or(false),
            shift: modifiers.shift,
        })
    }

    /// Publish `keyboard>>…` to event-socket subscribers when the lock/Shift
    /// state changed since the last publish.
    pub(crate) fn publish_keyboard_status(&mut self) {
        let Some(status) = self.keyboard_status() else {
            return;
        };
        if self.last_keyboard_status == Some(status) {
            return;
        }
        self.last_keyboard_status = Some(status);
        self.event_broadcaster.push_event(&status.event_payload());
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
        let trace = crate::compositor::runtime_flags::flags().wedge_trace;
        if trace {
            tracing::warn!(target: "beewm::wedge", "flush_focus_publish: begin");
        }
        self.publish_focused_window_state();
        if trace {
            tracing::warn!(target: "beewm::wedge", "flush_focus_publish: done");
        }
    }

    /// Assign keyboard focus to a window that was mapped during the last
    /// dispatch. MUST be called from the main loop AFTER `event_loop.dispatch()`,
    /// never from a dispatch callback — `set_keyboard_focus` re-enters the
    /// committing surface's cached state and would self-deadlock (see field doc
    /// on `pending_map_focus`).
    pub(crate) fn apply_pending_map_focus(&mut self) {
        if let Some(surface) = self.pending_map_focus.take() {
            let trace = crate::compositor::runtime_flags::flags().wedge_trace;
            if trace {
                tracing::warn!(target: "beewm::wedge", "apply_pending_map_focus: begin");
            }
            self.set_keyboard_focus(Some(surface));
            if trace {
                tracing::warn!(target: "beewm::wedge", "apply_pending_map_focus: done");
            }
        }
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
        // Tripwire for the deadlock documented above: this must never run while
        // a dispatch callback holds a surface's cached_state lock, because
        // `focused_window_title` re-enters `with_states`. Debug-only; compiled
        // out of release.
        #[cfg(debug_assertions)]
        DISPATCH_CALLBACK_DEPTH.with(|depth| {
            debug_assert_eq!(
                depth.get(),
                0,
                "publish_focused_window_state() called from inside a dispatch callback; \
                 use request_focus_publish() instead — re-entering with_states deadlocks the loop",
            );
        });

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
        self.event_broadcaster
            .push_event(&format!("window>>{title}"));
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

/// Decide whether the screen should blank now. Pure so the timeout policy is
/// unit-tested without a clock or DRM: blank only when a non-zero timeout has
/// elapsed, nothing already blanked, and no idle inhibitor is active.
fn should_blank(timeout_secs: u32, blanked: bool, inhibited: bool, idle: Duration) -> bool {
    timeout_secs > 0 && !blanked && !inhibited && idle.as_secs() >= timeout_secs as u64
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

#[cfg(debug_assertions)]
thread_local! {
    /// Nesting depth of in-progress Wayland/X11 dispatch callbacks that hold a
    /// surface's `cached_state` lock. Used only to back `DispatchCallbackGuard`.
    static DISPATCH_CALLBACK_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// RAII marker placed at the top of dispatch callbacks (focus_changed,
/// title_changed, X11 property_notify) that run while smithay holds a surface's
/// `cached_state` lock. While one is alive, `publish_focused_window_state`
/// would deadlock by re-entering `with_states`; it carries a `debug_assert!`
/// that fires if that ever happens, so the safe `request_focus_publish` path is
/// enforced in debug builds and tests. A no-op zero-sized type in release.
pub(crate) struct DispatchCallbackGuard;

impl DispatchCallbackGuard {
    #[must_use]
    pub(crate) fn enter() -> Self {
        #[cfg(debug_assertions)]
        DISPATCH_CALLBACK_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for DispatchCallbackGuard {
    fn drop(&mut self) {
        #[cfg(debug_assertions)]
        DISPATCH_CALLBACK_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

#[cfg(all(test, debug_assertions))]
mod dispatch_guard_tests {
    use super::{DISPATCH_CALLBACK_DEPTH, DispatchCallbackGuard};

    #[test]
    fn guard_tracks_nesting_depth_and_resets_on_drop() {
        DISPATCH_CALLBACK_DEPTH.with(|depth| assert_eq!(depth.get(), 0));
        {
            let _outer = DispatchCallbackGuard::enter();
            DISPATCH_CALLBACK_DEPTH.with(|depth| assert_eq!(depth.get(), 1));
            {
                let _inner = DispatchCallbackGuard::enter();
                DISPATCH_CALLBACK_DEPTH.with(|depth| assert_eq!(depth.get(), 2));
            }
            DISPATCH_CALLBACK_DEPTH.with(|depth| assert_eq!(depth.get(), 1));
        }
        DISPATCH_CALLBACK_DEPTH.with(|depth| assert_eq!(depth.get(), 0));
    }
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

#[cfg(test)]
mod idle_tests {
    use super::should_blank;
    use std::time::Duration;

    #[test]
    fn blanks_only_after_a_nonzero_timeout_elapses() {
        // Not yet elapsed.
        assert!(!should_blank(600, false, false, Duration::from_secs(599)));
        // Elapsed.
        assert!(should_blank(600, false, false, Duration::from_secs(600)));
    }

    #[test]
    fn timeout_zero_never_blanks() {
        assert!(!should_blank(0, false, false, Duration::from_secs(100_000)));
    }

    #[test]
    fn already_blanked_or_inhibited_does_not_blank_again() {
        assert!(!should_blank(60, true, false, Duration::from_secs(120)));
        assert!(!should_blank(60, false, true, Duration::from_secs(120)));
    }
}
