//! Background IPC threads. The UI thread never touches a socket.
//!
//! - The *request worker* owns one `Client`, executes queued commands in
//!   order and posts each response back to the UI event loop.
//! - The *event watcher* owns a second, subscribed connection. It reports
//!   connect/disconnect transitions and forwards low-rate `EngineEvent`s.
//!   It blocks on the socket while connected (no polling); while
//!   disconnected it retries once per second, only while the window is
//!   visible. Before subscribing it probes the engine's protocol version so
//!   an older Pulse engine is reported instead of failing obscurely.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use pulse_protocol::{Client, ClientError, Command, EngineEvent, PROTOCOL_VERSION, Response};

use crate::launcher::{self, Probe};

/// `PULSE_UI_TRACE=1` prints IPC jobs and replies to stderr (debugging aid).
pub fn trace_enabled() -> bool {
    static TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *TRACE.get_or_init(|| std::env::var_os("PULSE_UI_TRACE").is_some())
}

/// What the UI should do with a response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    Status,
    Config,
    Backends,
    ConfigSaved,
    Start,
    Stop,
    BackendSwitched,
    ActiveWindowForTarget,
    TargetSet,
    Quit,
}

pub struct Job {
    pub command: Command,
    pub tag: Tag,
}

pub enum UiMsg {
    Reply {
        tag: Tag,
        result: Result<Response, String>,
    },
    Connection {
        connected: bool,
        /// Nothing is listening on the socket (the engine is not running).
        not_running: bool,
        message: String,
    },
    /// An engine with a different protocol version is running.
    Mismatch {
        version: Option<u32>,
    },
    Event(EngineEvent),
}

/// Post a message to the UI thread. Returns false once the event loop is gone.
fn post(msg: UiMsg) -> bool {
    slint::invoke_from_event_loop(move || crate::controller::dispatch(msg)).is_ok()
}

fn describe(e: &ClientError) -> String {
    match e {
        ClientError::NotRunning(_) => "Pulse is not running".to_owned(),
        other => other.to_string(),
    }
}

pub fn spawn_request_worker(socket: PathBuf, jobs: Receiver<Job>) {
    let spawn = std::thread::Builder::new().name("pulse-ui-ipc".into()).spawn(move || {
        let mut client: Option<Client> = None;
        while let Ok(job) = jobs.recv() {
            if trace_enabled() {
                eprintln!("[ui-trace] job {:?}", job.tag);
            }
            if client.is_none() {
                match Client::connect(&socket) {
                    Ok(mut c) => {
                        // Switching the input method may take a couple of seconds.
                        c.set_timeout(Duration::from_secs(15));
                        client = Some(c);
                    }
                    Err(e) => {
                        if !post(UiMsg::Reply {
                            tag: job.tag,
                            result: Err(describe(&e)),
                        }) {
                            return;
                        }
                        continue;
                    }
                }
            }
            let Some(c) = client.as_mut() else { continue };
            let result = c.request(job.command).map_err(|e| {
                let msg = describe(&e);
                if matches!(
                    e,
                    ClientError::Io(_) | ClientError::Closed | ClientError::Timeout | ClientError::Protocol(_)
                ) {
                    client = None;
                }
                msg
            });
            if !post(UiMsg::Reply { tag: job.tag, result }) {
                return;
            }
        }
    });
    if let Err(e) = spawn {
        eprintln!("pulse-ui: cannot start IPC thread: {e}");
    }
}

/// Post a connection transition (only when it differs from the last one).
fn report(last: &mut Option<(bool, bool)>, connected: bool, not_running: bool, message: String) -> bool {
    if *last == Some((connected, not_running)) {
        return true;
    }
    *last = Some((connected, not_running));
    post(UiMsg::Connection {
        connected,
        not_running,
        message,
    })
}

pub fn spawn_event_watcher(socket: PathBuf, visible: Arc<AtomicBool>, quit: Arc<AtomicBool>) {
    let spawn = std::thread::Builder::new()
        .name("pulse-ui-events".into())
        .spawn(move || {
            let mut last: Option<(bool, bool)> = None;
            while !quit.load(Ordering::Relaxed) {
                if !visible.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                match launcher::probe(&socket) {
                    Probe::Version(v) if v == PROTOCOL_VERSION => {}
                    Probe::Version(v) => {
                        last = None;
                        if !post(UiMsg::Mismatch { version: Some(v) }) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    Probe::NotRunning => {
                        if !report(&mut last, false, true, "Pulse is not running".into()) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                    Probe::Failed(e) => {
                        if !report(&mut last, false, false, e) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                }
                let mut client = match Client::connect(&socket) {
                    Ok(c) => c,
                    Err(e) => {
                        let not_running = matches!(e, ClientError::NotRunning(_));
                        if !report(&mut last, false, not_running, describe(&e)) {
                            return;
                        }
                        std::thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };
                if let Err(e) = client.call(Command::Subscribe) {
                    if !report(&mut last, false, false, describe(&e)) {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
                if !report(&mut last, true, false, String::new()) {
                    return;
                }
                loop {
                    if quit.load(Ordering::Relaxed) {
                        return;
                    }
                    // Blocking read: no periodic wakeups while connected. The
                    // process exits on close, so no timeout is needed to stop.
                    match client.next_event(None) {
                        Ok(Some(ev)) => {
                            if !post(UiMsg::Event(ev)) {
                                return;
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            let not_running = matches!(e, ClientError::Closed | ClientError::Io(_));
                            if !report(&mut last, false, not_running, describe(&e)) {
                                return;
                            }
                            break;
                        }
                    }
                }
            }
        });
    if let Err(e) = spawn {
        eprintln!("pulse-ui: cannot start event thread: {e}");
    }
}
