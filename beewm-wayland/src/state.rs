use std::collections::HashMap;

use beewm_core::config::{Config, Keybind};
use beewm_core::layout::master_stack::MasterStack;
use beewm_core::layout::Layout;
use beewm_core::model::window::Geometry;
use beewm_core::model::workspace::Workspace;

use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::Id;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::Color32F;
use smithay::desktop::{Space, Window};
use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::input::keyboard::xkb;
use smithay::input::{Seat, SeatState};
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, SERIAL_COUNTER};
use smithay::wayland::compositor::{get_parent, CompositorClientState, CompositorState};
use smithay::wayland::dmabuf::{DmabufGlobal, DmabufState};
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::viewporter::ViewporterState;

use crate::cursor::CursorThemeManager;

/// Wrapper passed to calloop; holds compositor state + display handle.
pub struct CalloopData {
    pub state: Beewm,
    pub display_handle: DisplayHandle,
}

/// A keybinding pre-resolved at startup so the hot-path avoids string
/// allocations and repeated `xkb::keysym_from_name` lookups.
#[derive(Debug, Clone)]
pub struct ResolvedKeybind {
    pub logo: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub keysym: xkb::Keysym,
    pub action: beewm_core::config::Action,
}

/// The main compositor state.
pub struct Beewm {
    pub running: bool,
    pub config: Config,
    pub start_time: std::time::Instant,

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
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,

    // Pointer
    pub pointer_location: Point<f64, Logical>,
    pub cursor_status_serial: u64,
    pub cursor_status: CursorImageStatus,
    pub cursor_theme: CursorThemeManager,

    // Session (for VT switching in TTY mode)
    pub session: Option<Box<dyn std::any::Any>>,

    // Desktop management
    pub space: Space<Window>,
    pub layout: Box<dyn Layout>,
    pub workspaces: Vec<Workspace>,
    pub workspace_windows: Vec<Vec<Window>>,
    pub active_workspace: usize,
    /// Windows that have been created but not yet committed their first buffer.
    pub pending_windows: Vec<Window>,
    /// Root wl_surface -> mapped window lookup for commit-time surface routing.
    pub window_lookup: HashMap<WlSurface, Window>,
    /// Pre-allocated stable IDs for border elements (4 per window slot).
    /// Reused across frames so the DRM damage tracker sees unchanged geometry.
    pub border_ids: Vec<Id>,
    /// Global commit version for border elements; bumped whenever focus visuals change.
    pub border_commit_serial: u64,
    /// Set when visual state changed and a new frame should be rendered.
    pub needs_render: bool,
    /// Pre-resolved keybindings (no per-keypress string allocs).
    pub resolved_keybinds: Vec<ResolvedKeybind>,
    /// Cached border colours derived from config (avoid per-frame conversion).
    pub border_color_focused: Color32F,
    pub border_color_unfocused: Color32F,
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
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&display_handle);
        let single_pixel_buffer_state = SinglePixelBufferState::new::<Self>(&display_handle);
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
        let resolved_keybinds = resolve_keybinds(&config.keybinds);
        let border_color_focused = hex_to_color32f(config.border_color_focused);
        let border_color_unfocused = hex_to_color32f(config.border_color_unfocused);

        Self {
            running: true,
            config,
            start_time: std::time::Instant::now(),
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
            seat_state,
            seat,
            pointer_location: Point::from((0.0, 0.0)),
            cursor_status_serial: 0,
            cursor_status: CursorImageStatus::default_named(),
            cursor_theme: CursorThemeManager::new(),
            session: None,
            space: Space::default(),
            layout,
            workspaces: (0..num_ws).map(|_| Workspace::new()).collect(),
            workspace_windows: (0..num_ws).map(|_| Vec::new()).collect(),
            active_workspace: 0,
            pending_windows: Vec::new(),
            window_lookup: HashMap::new(),
            border_ids: Vec::new(),
            border_commit_serial: 0,
            needs_render: true,
            resolved_keybinds,
            border_color_focused,
            border_color_unfocused,
        }
    }

    pub fn window_index_for_surface(&self, workspace_idx: usize, surface: &WlSurface) -> Option<usize> {
        let surface_root = root_surface(surface);
        self.workspace_windows[workspace_idx]
            .iter()
            .position(|window| {
                window
                    .wl_surface()
                    .as_ref()
                    .map(|window_surface| **window_surface == surface_root)
                    .unwrap_or(false)
            })
    }

    pub fn mapped_window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.window_lookup.get(&root_surface(surface)).cloned()
    }

    pub fn track_window(&mut self, window: &Window) {
        if let Some(surface) = window.wl_surface().as_ref() {
            self.window_lookup.insert((**surface).clone(), window.clone());
        }
    }

    pub fn untrack_window_for_surface(&mut self, surface: &WlSurface) -> Option<Window> {
        self.window_lookup.remove(&root_surface(surface))
    }

    pub fn active_workspace_focused_index(&self) -> Option<usize> {
        match self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            Some(surface) => self.window_index_for_surface(self.active_workspace, &surface),
            None => self.workspaces[self.active_workspace].focused_idx,
        }
    }

    pub fn active_workspace_focused_window(&self) -> Option<&Window> {
        let idx = self.active_workspace_focused_index()?;
        self.workspace_windows[self.active_workspace].get(idx)
    }

    pub fn note_keyboard_focus_change(&mut self, focused: Option<&WlSurface>) {
        if let Some(surface) = focused {
            if let Some(idx) = self.window_index_for_surface(self.active_workspace, surface) {
                self.workspaces[self.active_workspace].focused_idx = Some(idx);
            }
        }

        self.border_commit_serial = self.border_commit_serial.wrapping_add(1);
        self.needs_render = true;
    }

    pub fn set_keyboard_focus(&mut self, focused: Option<WlSurface>) {
        let serial = SERIAL_COUNTER.next_serial();
        let keyboard = self.seat.get_keyboard().unwrap();
        keyboard.set_focus(self, focused.clone(), serial);

        // Smithay does not invoke SeatHandler::focus_changed when the focus is unset.
        if focused.is_none() {
            self.note_keyboard_focus_change(None);
        }
    }

    /// Build border render elements for all visible windows.
    pub fn border_elements(&mut self) -> Vec<SolidColorRenderElement> {
        let bw = self.config.border_width as i32;
        if bw == 0 {
            return Vec::new();
        }

        let focused_surface = self
            .seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus());

        let focused_color = self.border_color_focused;
        let unfocused_color = self.border_color_unfocused;

        let mut elements = Vec::new();
        let window_count = self.space.elements().count();

        // Ensure we have enough pre-allocated IDs (4 per window: top, bottom, left, right).
        let needed = window_count * 4;
        while self.border_ids.len() < needed {
            self.border_ids.push(Id::new());
        }

        for (win_idx, window) in self.space.elements().enumerate() {
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

            let commit =
                smithay::backend::renderer::utils::CommitCounter::from(self.border_commit_serial as usize);

            // Use stable pre-allocated IDs so the DRM compositor's damage
            // tracker can recognise unchanged elements across frames.
            let base = win_idx * 4;
            let border_top_id = self.border_ids[base].clone();
            let border_bottom_id = self.border_ids[base + 1].clone();
            let border_left_id = self.border_ids[base + 2].clone();
            let border_right_id = self.border_ids[base + 3].clone();

            // Top border
            elements.push(SolidColorRenderElement::new(
                border_top_id,
                Rectangle::new((x, y).into(), (w, bw).into()),
                commit,
                color,
                Kind::Unspecified,
            ));
            // Bottom border
            elements.push(SolidColorRenderElement::new(
                border_bottom_id,
                Rectangle::new((x, y + h - bw).into(), (w, bw).into()),
                commit,
                color,
                Kind::Unspecified,
            ));
            // Left border
            elements.push(SolidColorRenderElement::new(
                border_left_id,
                Rectangle::new((x, y + bw).into(), (bw, h - bw * 2).into()),
                commit,
                color,
                Kind::Unspecified,
            ));
            // Right border
            elements.push(SolidColorRenderElement::new(
                border_right_id,
                Rectangle::new((x + w - bw, y + bw).into(), (bw, h - bw * 2).into()),
                commit,
                color,
                Kind::Unspecified,
            ));
        }

        elements
    }

    pub fn effective_cursor_icon(&self) -> Option<CursorIcon> {
        match &self.cursor_status {
            CursorImageStatus::Hidden => None,
            CursorImageStatus::Named(icon) => Some(*icon),
            CursorImageStatus::Surface(_) => Some(CursorIcon::Default),
        }
    }

    pub fn set_cursor_status(&mut self, status: CursorImageStatus) {
        self.cursor_status = status;
        self.cursor_status_serial = self.cursor_status_serial.wrapping_add(1);
        self.needs_render = true;
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

            let location = Point::from((x, y));
            if self.space.element_location(window) != Some(location) {
                self.space.map_element(window.clone(), location, false);
            }
        }
        self.needs_render = true;
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

        self.needs_render = true;
        self.relayout();

        // Focus the active window on the new workspace
        let focus = self.workspaces[self.active_workspace]
            .focused_idx
            .and_then(|focus_idx| self.workspace_windows[self.active_workspace].get(focus_idx))
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
        if let Some(focus) = focus {
            self.set_keyboard_focus(Some(focus));
        } else {
            // No windows — clear keyboard focus
            self.set_keyboard_focus(None);
        }
    }

    /// Move the focused window to another workspace.
    pub fn move_to_workspace(&mut self, target: usize) {
        if target >= self.workspaces.len() || target == self.active_workspace {
            return;
        }

        let focus_idx = match self.active_workspace_focused_index() {
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
        let focus = self.workspaces[self.active_workspace]
            .focused_idx
            .and_then(|focus_idx| self.workspace_windows[self.active_workspace].get(focus_idx))
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
        if let Some(focus) = focus {
            self.set_keyboard_focus(Some(focus));
        } else {
            self.set_keyboard_focus(None);
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

fn root_surface(surface: &WlSurface) -> WlSurface {
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
