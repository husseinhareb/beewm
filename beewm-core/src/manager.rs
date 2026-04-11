use crate::config::Config;
use crate::handler::handle_event;
use crate::state::State;
use crate::DisplayServer;

/// The main WM manager that ties together state and display server.
pub struct Manager<S: DisplayServer> {
    pub state: State<S::Handle>,
    pub server: S,
}

impl<S: DisplayServer> Manager<S> {
    pub fn new(server: S, config: Config) -> Self {
        Self {
            state: State::new(config),
            server,
        }
    }

    /// Run the main event loop. Returns when the WM should exit.
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        use tokio::io::unix::AsyncFd;

        let fd = self.server.as_fd();
        let async_fd = AsyncFd::new(fd)?;

        loop {
            // Wait for the fd to become readable
            let mut guard = async_fd.readable().await?;

            // Drain all available events
            while let Ok(event) = self.server.next_event() {
                let should_continue = handle_event(&mut self.state, event);
                if !should_continue {
                    tracing::info!("Quit requested, shutting down");
                    return Ok(());
                }
            }

            // Flush all queued actions
            let actions: Vec<_> = self.state.actions.drain(..).collect();
            for action in actions {
                self.server.execute(action)?;
            }
            self.server.flush()?;

            guard.clear_ready();
        }
    }
}
