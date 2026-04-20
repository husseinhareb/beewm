mod cursor;
mod decorations;
mod focus;
mod tiling;
mod workspace;

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::sync::Fence;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::keyboard::xkb;
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::input::{Seat, SeatState};
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{Clock, Logical, Monotonic, Point, Size};
use smithay::wayland::compositor::{
    CompositorClientState, CompositorState, add_blocker, add_pre_commit_hook, get_parent,
    with_states,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::drm_syncobj::{DrmSyncPointSource, DrmSyncobjCachedState, DrmSyncobjState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;

use crate::config::{Action, Config, Keybind, LayoutKind};
use crate::layout::Layout;
use crate::layout::dwindle::Dwindle;
use crate::layout::master_stack::MasterStack;
use crate::model::workspace::Workspace;

use super::commands::ChildEnvironment;

use super::cursor::CursorThemeManager;

pub use self::decorations::{
    expand_by_border, root_is_swap_highlighted, visible_border_rectangles,
    window_border_overlaps_layer,
};
pub use self::tiling::DwindleTree;
pub use self::workspace::{FloatToggleTransition, float_toggle_transition};

const ACTIVE_WORKSPACE_STATE_PATH: &str = "/tmp/beewm_workspace";
const WORKSPACE_STATE_PATH: &str = "/tmp/beewm_workspaces";
static STATE_FILE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

type SyncobjBlockerInstaller = dyn Fn(DrmSyncPointSource, Client);

/// A keybinding pre-resolved at startup so the hot-path avoids string
/// allocations and repeated `xkb::keysym_from_name` lookups.
#[derive(Debug, Clone)]
pub struct ResolvedKeybind {
    pub logo: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub keysym: xkb::Keysym,
    pub action: Action,
}

/// State for an in-progress floating window move (Super + LMB drag).
#[derive(Debug, Clone)]
pub struct MoveGrab {
    /// The window being moved.
    pub window: Window,
    /// Pointer position when the grab started.
    pub start_pointer: Point<f64, Logical>,
    /// Window position when the grab started.
    pub start_window_pos: Point<i32, Logical>,
}

/// State for an in-progress tiled-window swap grab (Super + LMB drag).
#[derive(Debug, Clone)]
pub struct TiledSwapGrab {
    /// The tiled window being dragged.
    pub window: Window,
    /// Workspace that owns the dragged tiled window.
    pub workspace_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeHorizontalEdge {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeVerticalEdge {
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResizeEdges {
    pub horizontal: ResizeHorizontalEdge,
    pub vertical: ResizeVerticalEdge,
}

impl ResizeEdges {
    pub fn cursor_icon(self) -> CursorIcon {
        match (self.vertical, self.horizontal) {
            (ResizeVerticalEdge::Top, ResizeHorizontalEdge::Left) => CursorIcon::NwResize,
            (ResizeVerticalEdge::Top, ResizeHorizontalEdge::Right) => CursorIcon::NeResize,
            (ResizeVerticalEdge::Bottom, ResizeHorizontalEdge::Left) => CursorIcon::SwResize,
            (ResizeVerticalEdge::Bottom, ResizeHorizontalEdge::Right) => CursorIcon::SeResize,
        }
    }
}

/// State for an in-progress floating window resize (Super + RMB drag).
#[derive(Debug, Clone)]
pub struct ResizeGrab {
    /// The window being resized.
    pub window: Window,
    /// Pointer position when the resize started.
    pub start_pointer: Point<f64, Logical>,
    /// Window position when the resize started.
    pub start_window_pos: Point<i32, Logical>,
    /// Window size when the resize started.
    pub start_window_size: Size<i32, Logical>,
    /// Which edges of the window are following the pointer.
    pub edges: ResizeEdges,
    /// Latest requested window position during the interactive resize.
    pub current_window_pos: Point<i32, Logical>,
    /// Latest requested window size during the interactive resize.
    pub current_window_size: Size<i32, Logical>,
}

#[derive(Debug, Clone, Copy)]
pub struct FloatingWindowData {
    pub position: Point<i32, Logical>,
    pub size: Size<i32, Logical>,
}

impl FloatingWindowData {
    pub fn new(position: Point<i32, Logical>, size: Size<i32, Logical>) -> Self {
        Self {
            position,
            size: Size::from((size.w.max(1), size.h.max(1))),
        }
    }
}

/// The main compositor state.
pub struct Beewm {
    pub running: bool,
    pub config: Config,
    pub start_time: std::time::Instant,
    pub display_handle: DisplayHandle,

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub _xdg_decoration_state: XdgDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub shm_state: ShmState,
    pub _output_manager_state: OutputManagerState,
    pub _viewporter_state: ViewporterState,
    pub _fractional_scale_manager_state: FractionalScaleManagerState,
    pub _single_pixel_buffer_state: SinglePixelBufferState,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
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

    // Session (for VT switching in TTY mode)
    pub session: Option<LibSeatSession>,

    // Desktop management
    pub space: Space<Window>,
    pub layout: Box<dyn Layout>,
    pub workspaces: Vec<Workspace>,
    pub workspace_windows: Vec<Vec<Window>>,
    pub(crate) dwindle_trees: Vec<DwindleTree<WlSurface>>,
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
    /// The window currently occupying the full screen, if any.
    pub fullscreen_window: Option<Window>,
    /// Tracks popup surfaces and provides grab support.
    pub popup_manager: PopupManager,
    /// Floating windows (not subject to tiling) mapped to their last geometry.
    /// The key is the root WlSurface; the value is where the window is placed
    /// and how large it should be when restored.
    pub floating_windows: HashMap<WlSurface, FloatingWindowData>,
    /// Active floating-window move grab (Super + left-click drag).
    pub move_grab: Option<MoveGrab>,
    /// Active tiled-window swap grab (Super + left-click drag).
    pub tiled_swap_grab: Option<TiledSwapGrab>,
    /// Current tiled-window swap drop target, if the pointer is over one.
    pub tiled_swap_target: Option<WlSurface>,
    /// Active floating-window resize grab (Super + right-click drag).
    pub resize_grab: Option<ResizeGrab>,
    /// Pre-resolved keybindings (no per-keypress string allocs).
    pub resolved_keybinds: Vec<ResolvedKeybind>,
    /// Cached border colours derived from config (avoid per-frame conversion).
    pub border_color_focused: Color32F,
    pub border_color_unfocused: Color32F,
    /// Installs acquire-fence event sources into the active backend loop.
    pub syncobj_blocker_installer: Option<Box<SyncobjBlockerInstaller>>,
    /// Compositor-specific environment for spawned child processes.
    pub(crate) child_env: ChildEnvironment,
}

impl Beewm {
    pub fn new(display: &Display<Self>, config: Config) -> Self {
        let display_handle = display.handle();

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let viewporter_state = ViewporterState::new::<Self>(&display_handle);
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<Self>(&display_handle);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&display_handle);
        let data_device_state = DataDeviceState::new::<Self>(&display_handle);
        let primary_selection_state = PrimarySelectionState::new::<Self>(&display_handle);
        let dmabuf_state = DmabufState::new();
        let presentation_clock = Clock::<Monotonic>::new();
        let presentation_state =
            PresentationState::new::<Self>(&display_handle, presentation_clock.id() as u32);
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "beewm");

        // Initialize keyboard and pointer on the seat
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("Failed to add keyboard");
        seat.add_pointer();

        let num_ws = config.num_workspaces;
        let layout = build_layout(&config);
        let resolved_keybinds = resolve_keybinds(&config.keybinds);
        let border_color_focused = hex_to_color32f(config.border_color_focused);
        let border_color_unfocused = hex_to_color32f(config.border_color_unfocused);
        let cursor_shape_manager_state_ = CursorShapeManagerState::new::<Self>(&display_handle);

        let state = Self {
            running: true,
            config,
            start_time: std::time::Instant::now(),
            display_handle: display_handle.clone(),
            compositor_state,
            xdg_shell_state,
            _xdg_decoration_state: xdg_decoration_state,
            layer_shell_state,
            shm_state,
            _output_manager_state: output_manager_state,
            _viewporter_state: viewporter_state,
            _fractional_scale_manager_state: fractional_scale_manager_state,
            _single_pixel_buffer_state: single_pixel_buffer_state,
            data_device_state,
            primary_selection_state,
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
            session: None,
            space: Space::default(),
            layout,
            workspaces: (0..num_ws).map(|_| Workspace::default()).collect(),
            workspace_windows: (0..num_ws).map(|_| Vec::new()).collect(),
            dwindle_trees: (0..num_ws).map(|_| DwindleTree::default()).collect(),
            active_workspace: 0,
            pending_windows: Vec::new(),
            window_lookup: HashMap::new(),
            border_ids: Vec::new(),
            border_commit_serial: 0,
            needs_render: true,
            fullscreen_window: None,
            popup_manager: PopupManager::default(),
            floating_windows: HashMap::new(),
            move_grab: None,
            tiled_swap_grab: None,
            tiled_swap_target: None,
            resize_grab: None,
            resolved_keybinds,
            border_color_focused,
            border_color_unfocused,
            syncobj_blocker_installer: None,
            child_env: ChildEnvironment::default(),
        };

        state.publish_workspace_state();
        state
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
                }
                Err(error) => {
                    tracing::warn!("Failed to install explicit-sync blocker: {}", error);
                }
            }
        });
    }

    pub(crate) fn publish_workspace_state(&self) {
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
    }
}

pub fn active_workspace_state_contents(active_workspace: usize) -> String {
    (active_workspace + 1).to_string()
}

pub fn workspace_state_contents(active_workspace: usize, workspaces: &[Workspace]) -> String {
    let occupied = workspaces
        .iter()
        .enumerate()
        .filter(|(_, workspace)| workspace.window_count > 0)
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

fn build_layout(config: &Config) -> Box<dyn Layout> {
    match config.layout {
        LayoutKind::Dwindle => Box::new(Dwindle {
            split_ratio: config.split_ratio,
        }),
        LayoutKind::MasterStack => Box::new(MasterStack {
            master_ratio: config.split_ratio,
        }),
    }
}

/// Convert a 0xRRGGBB hex color to smithay's Color32F (with alpha=1.0).
fn hex_to_color32f(hex: u32) -> Color32F {
    let r = ((hex >> 16) & 0xFF) as f32 / 255.0;
    let g = ((hex >> 8) & 0xFF) as f32 / 255.0;
    let b = (hex & 0xFF) as f32 / 255.0;
    Color32F::new(r, g, b, 1.0)
}

pub(super) fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

/// Pre-resolve keybinds so the hot-path is a simple integer comparison.
fn resolve_keybinds(keybinds: &[Keybind]) -> Vec<ResolvedKeybind> {
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
            ResolvedKeybind {
                logo,
                shift,
                ctrl,
                alt,
                keysym,
                action: bind.action.clone(),
            }
        })
        .collect()
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
