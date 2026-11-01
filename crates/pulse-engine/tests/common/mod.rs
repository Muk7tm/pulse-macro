//! In-process engine harness: temporary config/runtime directories, a mock
//! backend factory with failure injection, and a socket client.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use pulse_config::Paths;
use pulse_core::{BackendKind, EngineState, HeldInputs, LogicalInput};
use pulse_engine::{Engine, EngineOptions};
use pulse_input::mock::{MockBackend, MockLog};
use pulse_input::{InputBackend, InputError};
use pulse_protocol::{Client, Command, ResponseData, StatusInfo};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub type Logs = Arc<Mutex<Vec<(BackendKind, Arc<Mutex<MockLog>>)>>>;

#[derive(Default)]
pub struct Faults {
    /// Backends whose construction fails.
    pub fail_open: Vec<BackendKind>,
    /// Newly opened backends fail after this many batches.
    pub fail_after_batches: Option<u64>,
}

pub struct TestEngine {
    pub root: PathBuf,
    pub paths: Paths,
    pub logs: Logs,
    pub faults: Arc<Mutex<Faults>>,
    pub opened: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

pub struct Setup<'a> {
    pub name: &'a str,
    pub config: &'a str,
    pub display: Option<&'a str>,
    pub xtest_is_physical: bool,
    pub root: Option<PathBuf>,
    /// Backends that fail to open from the very first attempt.
    pub fail_open: Vec<BackendKind>,
}

impl Default for Setup<'_> {
    fn default() -> Self {
        Setup {
            name: "t",
            config: "schema_version = 2\n[target]\nrequire_focus = false\n",
            display: None,
            xtest_is_physical: false,
            root: None,
            fail_open: Vec::new(),
        }
    }
}

pub fn fresh_root(name: &str) -> PathBuf {
    // Short path: Unix socket paths are limited to 108 bytes.
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = PathBuf::from(format!(
        "/tmp/pulse-t{}-{n}-{}",
        std::process::id(),
        &name[..name.len().min(8)]
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("cfg")).expect("mkdir");
    root
}

impl TestEngine {
    pub fn start(setup: Setup<'_>) -> TestEngine {
        let root = setup.root.clone().unwrap_or_else(|| fresh_root(setup.name));
        let cfg_file = root.join("cfg/config.toml");
        if !cfg_file.exists() {
            std::fs::write(&cfg_file, setup.config).expect("write config");
        }
        let paths = Paths::with_roots(root.join("cfg"), root.join("run"));
        let logs: Logs = Arc::default();
        let faults = Arc::new(Mutex::new(Faults {
            fail_open: setup.fail_open.clone(),
            fail_after_batches: None,
        }));
        let opened = Arc::new(AtomicUsize::new(0));
        let (logs2, faults2, opened2) = (Arc::clone(&logs), Arc::clone(&faults), Arc::clone(&opened));
        let factory = Box::new(move |kind: BackendKind| -> Result<Box<dyn InputBackend>, InputError> {
            let f = faults2.lock().unwrap_or_else(|p| p.into_inner());
            if f.fail_open.contains(&kind) {
                return Err(InputError::DeviceInit(format!("injected open failure for {kind}")));
            }
            let (mut b, log) = MockBackend::new(kind);
            b.fail_after_batches = f.fail_after_batches;
            opened2.fetch_add(1, Ordering::SeqCst);
            logs2.lock().unwrap_or_else(|p| p.into_inner()).push((kind, log));
            Ok(Box::new(b))
        });
        let opts = EngineOptions {
            paths: paths.clone(),
            display: setup.display.map(str::to_owned),
            allow_xwayland: false,
            disable_x11: setup.display.is_none(),
            backend_factory: Some(factory),
            xtest_hotkeys_are_physical: setup.xtest_is_physical,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || match Engine::new(opts) {
            Ok(e) => {
                let _ = tx.send(Ok(()));
                e.run().expect("engine loop");
            }
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
            }
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("engine start timeout")
            .expect("engine start");
        TestEngine {
            root,
            paths,
            logs,
            faults,
            opened,
            thread: Some(thread),
        }
    }

    pub fn try_start(setup: Setup<'_>) -> Result<(), String> {
        let root = setup.root.clone().unwrap_or_else(|| fresh_root(setup.name));
        let paths = Paths::with_roots(root.join("cfg"), root.join("run"));
        Engine::new(EngineOptions {
            paths,
            display: None,
            allow_xwayland: false,
            disable_x11: true,
            backend_factory: Some(Box::new(|k| {
                Ok(Box::new(MockBackend::new(k).0) as Box<dyn InputBackend>)
            })),
            xtest_hotkeys_are_physical: false,
        })
        .map(drop)
        .map_err(|e| e.to_string())
    }

    pub fn client(&self) -> Client {
        let mut c = Client::connect(&self.paths.socket()).expect("connect");
        c.set_timeout(Duration::from_secs(10));
        c
    }

    pub fn status(&self) -> StatusInfo {
        match self.client().call_data(Command::Status).expect("status") {
            Some(ResponseData::Status(s)) => *s,
            other => panic!("unexpected {other:?}"),
        }
    }

    pub fn wait_state(&self, want: EngineState, timeout: Duration) -> StatusInfo {
        let deadline = Instant::now() + timeout;
        loop {
            let s = self.status();
            if s.state == want || Instant::now() > deadline {
                assert_eq!(s.state, want, "timed out waiting for {want}: {s:?}");
                return s;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Events recorded by the most recently opened backend of `kind`.
    pub fn events(&self, kind: BackendKind) -> Vec<LogicalInput> {
        let logs = self.logs.lock().unwrap_or_else(|p| p.into_inner());
        logs.iter()
            .rev()
            .find(|(k, _)| *k == kind)
            .map(|(_, l)| {
                l.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .events
                    .iter()
                    .map(|(_, e)| *e)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn backend_dropped(&self, index: usize) -> bool {
        let logs = self.logs.lock().unwrap_or_else(|p| p.into_inner());
        logs.get(index)
            .is_some_and(|(_, l)| l.lock().unwrap_or_else(|p| p.into_inner()).dropped)
    }

    pub fn shutdown(mut self) {
        let _ = self.client().request(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            t.join().expect("engine thread");
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }

    /// Shut down but keep the directories (for restart tests).
    pub fn shutdown_keep(mut self) -> PathBuf {
        let _ = self.client().request(Command::Shutdown);
        if let Some(t) = self.thread.take() {
            t.join().expect("engine thread");
        }
        self.root.clone()
    }
}

/// Validate a press/release stream: no double press, no orphan release,
/// nothing held at the end.
pub fn assert_balanced(events: &[LogicalInput]) {
    let mut held = HeldInputs::new();
    for e in events {
        // Releases of never-pressed inputs are allowed: cleanup is
        // conservative and the OS ignores them.
        if let (p, pulse_core::KeyState::Pressed) = e.pressable() {
            assert!(!held.is_held(p), "double press of {p}");
        }
        held.apply_lenient(e);
    }
    assert!(held.is_empty(), "inputs left held: {held:?}");
}

pub fn clicks(events: &[LogicalInput]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                LogicalInput::MouseButton {
                    state: pulse_core::KeyState::Pressed,
                    ..
                }
            )
        })
        .count()
}
