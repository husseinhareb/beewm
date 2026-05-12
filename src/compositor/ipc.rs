use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;

use smithay::reexports::calloop::channel::{self, Channel, Sender};

use crate::compositor::state::Beewm;

const CONTROL_SOCKET_NAME: &str = "beewm-control.sock";
const CONTROL_SOCKET_FALLBACK: &str = "/tmp/beewm-control.sock";
const EVENT_SOCKET_NAME: &str = "beewm-events.sock";
const EVENT_SOCKET_FALLBACK: &str = "/tmp/beewm-events.sock";

pub enum Command {
    SwitchWorkspace(u32),
}

pub struct IpcServer {
    path: PathBuf,
    _thread: thread::JoinHandle<()>,
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Bind the control socket and start accepting commands.
pub fn start() -> io::Result<(IpcServer, Channel<Command>)> {
    let path = control_socket_path();
    if path.exists() {
        fs::remove_file(&path)?;
    }

    let listener = UnixListener::bind(&path)?;
    let (sender, channel) = channel::channel();
    let thread_path = path.clone();
    let thread = thread::Builder::new()
        .name("beewm-ipc".into())
        .spawn(move || accept_control_loop(listener, sender, thread_path))?;

    Ok((IpcServer { path, _thread: thread }, channel))
}

/// Bind the event socket and start accepting subscriber connections.
///
/// Each accepted [`UnixStream`] is forwarded to the compositor's main loop
/// via the returned channel so it can be stored and written to directly
/// whenever state changes (focused window, active workspace, etc.).
pub fn start_event_listener() -> io::Result<(IpcServer, Channel<UnixStream>)> {
    let path = event_socket_path();
    if path.exists() {
        fs::remove_file(&path)?;
    }

    let listener = UnixListener::bind(&path)?;
    let (sender, channel) = channel::channel();
    let thread_path = path.clone();
    let thread = thread::Builder::new()
        .name("beewm-events-accept".into())
        .spawn(move || accept_event_loop(listener, sender, thread_path))?;

    Ok((IpcServer { path, _thread: thread }, channel))
}

pub fn apply_command(state: &mut Beewm, command: Command) {
    match command {
        Command::SwitchWorkspace(number) if number >= 1 => {
            state.switch_workspace((number - 1) as usize);
        }
        Command::SwitchWorkspace(_) => {}
    }
}

/// Path for the event socket — the read-only push channel for subscribers.
pub fn event_socket_path() -> PathBuf {
    socket_path(EVENT_SOCKET_NAME, EVENT_SOCKET_FALLBACK)
}

fn control_socket_path() -> PathBuf {
    socket_path(CONTROL_SOCKET_NAME, CONTROL_SOCKET_FALLBACK)
}

fn socket_path(name: &str, fallback: &str) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .map(|dir| dir.join(name))
        .unwrap_or_else(|| PathBuf::from(fallback))
}

fn accept_control_loop(listener: UnixListener, sender: Sender<Command>, path: PathBuf) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(command) = read_command(stream) {
                    if sender.send(command).is_err() {
                        break;
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    "Workspace control socket {} stopped accepting commands: {}",
                    path.display(),
                    error
                );
                break;
            }
        }
    }
}

fn accept_event_loop(listener: UnixListener, sender: Sender<UnixStream>, path: PathBuf) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if sender.send(stream).is_err() {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(
                    "Event socket {} stopped accepting connections: {}",
                    path.display(),
                    error
                );
                break;
            }
        }
    }
}

fn read_command(stream: UnixStream) -> Option<Command> {
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    match reader.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => parse_command(&line),
        Err(error) => {
            tracing::warn!("Failed to read workspace control command: {}", error);
            None
        }
    }
}

fn parse_command(line: &str) -> Option<Command> {
    let mut parts = line.split_whitespace();
    let command = parts.next()?;
    match command {
        "workspace" => {
            let number = parts.next()?.parse::<u32>().ok()?;
            Some(Command::SwitchWorkspace(number))
        }
        other => {
            tracing::debug!("Ignoring unknown IPC command '{}'", other);
            None
        }
    }
}
