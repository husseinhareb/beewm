use std::fs;
use std::io::{self, BufRead, BufReader};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::thread;

use smithay::reexports::calloop::channel::{self, Channel, Sender};

use crate::compositor::state::Beewm;

const CONTROL_SOCKET_NAME: &str = "beewm-control.sock";
const CONTROL_SOCKET_FALLBACK: &str = "/tmp/beewm-control.sock";

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
        .spawn(move || accept_loop(listener, sender, thread_path))?;

    Ok((
        IpcServer {
            path,
            _thread: thread,
        },
        channel,
    ))
}

pub fn apply_command(state: &mut Beewm, command: Command) {
    match command {
        Command::SwitchWorkspace(number) if number >= 1 => {
            state.switch_workspace((number - 1) as usize);
        }
        Command::SwitchWorkspace(_) => {}
    }
}

fn accept_loop(listener: UnixListener, sender: Sender<Command>, path: PathBuf) {
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

fn control_socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join(CONTROL_SOCKET_NAME))
        .unwrap_or_else(|| PathBuf::from(CONTROL_SOCKET_FALLBACK))
}
