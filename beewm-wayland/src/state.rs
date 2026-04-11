use std::time::Duration;

use beewm_core::config::Config;
use beewm_core::layout::master_stack::MasterStack;
use beewm_core::layout::Layout;
use beewm_core::model::window::Geometry;
use beewm_core::model::workspace::Workspace;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::glow::GlowRenderer;
use smithay::backend::winit::{self, WinitEvent};
use smithay::desktop::{Space, Window};
use smithay::input::{Seat, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, PostAction};
use smithay::reexports::wayland_server::backend::ClientData;
use smithay::reexports::wayland_server::{Display, DisplayHandle};
use smithay::utils::{Size, Transform};
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

/// Wrapper passed to calloop; holds compositor state + display handle.
pub struct CalloopData {
    pub state: Beewm,
    pub display_handle: DisplayHandle,
}

/// The main compositor state.
pub struct Beewm {
    pub display_handle: DisplayHandle,
    pub running: bool,
    pub config: Config,

    // Smithay protocol state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub seat: Seat<Self>,

    // Desktop management
    pub space: Space<Window>,
    pub layout: Box<dyn Layout>,
    pub workspaces: Vec<Workspace>,
    pub active_workspace: usize,
}

impl Beewm {
    fn new(display: &Display<Self>, config: Config) -> Self {
        let display_handle = display.handle();

        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
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
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            seat,
            space: Space::default(),
            layout,
            workspaces: (0..num_ws).map(Workspace::new).collect(),
            active_workspace: 0,
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

        let windows: Vec<Window> = self.space.elements().cloned().collect();
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

/// The public Wayland backend entry point.
pub struct WaylandBackend;

impl WaylandBackend {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self)
    }

    /// Run the compositor with the winit backend (opens a window in current session).
    pub fn run(self, config: Config) -> Result<(), Box<dyn std::error::Error>> {
        let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new()?;
        let display: Display<Beewm> = Display::new()?;
        let display_handle = display.handle();

        let state = Beewm::new(&display, config);

        let mut data = CalloopData {
            state,
            display_handle: display_handle.clone(),
        };

        // Set up the Wayland listening socket
        let listening_socket = ListeningSocketSource::new_auto()?;
        let socket_name = listening_socket.socket_name().to_os_string();
        tracing::info!("Wayland socket: {:?}", socket_name);

        event_loop.handle().insert_source(
            listening_socket,
            |client_stream, _, data| {
                if let Err(e) = data.display_handle.insert_client(
                    client_stream,
                    std::sync::Arc::new(ClientState::default()),
                ) {
                    tracing::error!("Failed to insert client: {}", e);
                }
            },
        )?;

        // Insert the Display into the event loop so wayland clients are dispatched
        event_loop.handle().insert_source(
            Generic::new(
                display,
                Interest::READ,
                smithay::reexports::calloop::Mode::Level,
            ),
            |_, display, data| {
                // Safety: we don't drop the display
                unsafe {
                    display
                        .get_mut()
                        .dispatch_clients(&mut data.state)
                        .unwrap();
                }
                Ok(PostAction::Continue)
            },
        )?;

        // Initialize winit backend
        let (mut winit_backend, winit_evt) = winit::init::<GlowRenderer>()?;

        // Create the output
        let mode = Mode {
            size: winit_backend.window_size(),
            refresh: 60_000,
        };

        let output = Output::new(
            "winit".to_string(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "beewm".into(),
                model: "winit".into(),
            },
        );

        output.create_global::<Beewm>(&display_handle);
        output.change_current_state(
            Some(mode),
            Some(Transform::Flipped180),
            None,
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        data.state.space.map_output(&output, (0, 0));

        let mut damage_tracker = OutputDamageTracker::from_output(&output);

        // Insert winit event source
        event_loop
            .handle()
            .insert_source(winit_evt, move |event, _, data| match event {
                WinitEvent::Resized { size, .. } => {
                    let mode = Mode {
                        size,
                        refresh: 60_000,
                    };
                    if let Some(output) = data.state.space.outputs().next().cloned() {
                        output.change_current_state(Some(mode), None, None, None);
                    }
                    data.state.relayout();
                }
                WinitEvent::Input(event) => {
                    crate::input::handle_input(&mut data.state, event);
                }
                WinitEvent::Focus(_) | WinitEvent::Redraw => {}
                WinitEvent::CloseRequested => {
                    data.state.running = false;
                }
            })?;

        std::env::set_var("WAYLAND_DISPLAY", &socket_name);

        tracing::info!("Starting event loop");

        while data.state.running {
            // Render
            let output = data.state.space.outputs().next().cloned();
            if let Some(ref output) = output {
                if let Ok((renderer, mut framebuffer)) = winit_backend.bind() {
                    let elements = data
                        .state
                        .space
                        .render_elements_for_output(renderer, output, 1.0)
                        .unwrap_or_default();

                    damage_tracker
                        .render_output(
                            renderer,
                            &mut framebuffer,
                            0,
                            &elements,
                            [0.1, 0.1, 0.1, 1.0],
                        )
                        .ok();
                }
                winit_backend.submit(None).ok();
            }

            // Dispatch event loop
            let timeout = Duration::from_millis(16);
            event_loop.dispatch(Some(timeout), &mut data)?;

            // Process pending surface state
            data.state.space.refresh();
        }

        Ok(())
    }
}
