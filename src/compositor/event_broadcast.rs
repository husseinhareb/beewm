use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{self, Sender};
use std::thread;

enum Msg {
    /// Deliver `event\n` to every connected subscriber.
    Event(String),
    /// Register a new subscriber and immediately deliver `initial` to it.
    NewSubscriber(UnixStream, String),
}

/// Receives event strings from the compositor's main thread and forwards them
/// to all connected beebar/tool subscribers via Unix-socket writes — all I/O
/// is confined to the background thread so the compositor event loop is never
/// blocked.
pub struct EventBroadcaster {
    sender: Option<Sender<Msg>>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl EventBroadcaster {
    pub fn new() -> Self {
        if super::runtime_flags::flags().event_broadcaster_disabled {
            return Self {
                sender: None,
                _thread: None,
            };
        }
        let (sender, receiver) = mpsc::channel::<Msg>();
        let thread = thread::Builder::new()
            .name("beewm-event-broadcast".into())
            .spawn(move || {
                let mut subscribers: Vec<UnixStream> = Vec::new();
                while let Ok(msg) = receiver.recv() {
                    match msg {
                        Msg::NewSubscriber(mut stream, initial) => {
                            if let Err(error) = stream.set_nonblocking(true) {
                                tracing::warn!(
                                    "Failed to set event subscriber to non-blocking: {}",
                                    error
                                );
                                continue;
                            }
                            if stream.write_all(initial.as_bytes()).is_ok() {
                                subscribers.push(stream);
                            }
                        }
                        Msg::Event(payload) => {
                            subscribers.retain_mut(|s| s.write_all(payload.as_bytes()).is_ok());
                        }
                    }
                }
            })
            .expect("failed to spawn event-broadcast thread");
        Self {
            sender: Some(sender),
            _thread: Some(thread),
        }
    }

    /// Queue `event\n` for delivery to all subscribers. Never blocks.
    pub fn push_event(&self, event: &str) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        let mut payload = String::with_capacity(event.len() + 1);
        payload.push_str(event);
        payload.push('\n');
        let _ = sender.send(Msg::Event(payload));
    }

    /// Register `stream` as a subscriber and queue `initial` as its first
    /// message. Never blocks.
    pub fn add_subscriber(&self, stream: UnixStream, initial: String) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        let _ = sender.send(Msg::NewSubscriber(stream, initial));
    }
}
