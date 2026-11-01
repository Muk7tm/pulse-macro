//! Process-level robustness with the real engine binary and the real XTest
//! backend on an isolated X server (`PULSE_TEST_DISPLAY`, e.g. Xvfb `:99`).
//!
//! - SIGTERM while a button is held: the engine releases it and exits 0.
//! - SIGKILL while a button is held: documents the unavoidable limitation
//!   that X11 keeps XTest-pressed state when the process cannot clean up.

use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, Command as Proc, Stdio};
use std::time::{Duration, Instant};

use pulse_protocol::{Client, Command};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, KeyButMask, WindowClass};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn display() -> Option<String> {
    match std::env::var("PULSE_TEST_DISPLAY") {
        Ok(d) if !d.is_empty() => Some(d),
        _ => {
            eprintln!("SKIPPED: set PULSE_TEST_DISPLAY to an isolated X server (e.g. Xvfb :99)");
            None
        }
    }
}

struct Receiver {
    conn: RustConnection,
    win: u32,
}

impl Receiver {
    fn new(display: &str) -> Receiver {
        let (conn, screen) = x11rb::connect(Some(display)).expect("connect");
        let s = conn.setup().roots[screen].clone();
        let win = conn.generate_id().expect("id");
        conn.create_window(
            s.root_depth,
            win,
            s.root,
            0,
            0,
            300,
            300,
            0,
            WindowClass::INPUT_OUTPUT,
            s.root_visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE),
        )
        .expect("create");
        conn.map_window(win).expect("map");
        conn.warp_pointer(x11rb::NONE, win, 0, 0, 0, 0, 150, 150).expect("warp");
        conn.sync().expect("sync");
        std::thread::sleep(Duration::from_millis(50));
        while let Ok(Some(_)) = conn.poll_for_event() {}
        Receiver { conn, win }
    }

    fn wait(&self, press: bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.conn.poll_for_event().expect("poll") {
                Some(Event::ButtonPress(e)) if press && e.event == self.win && e.detail == 1 => return true,
                Some(Event::ButtonRelease(e)) if !press && e.event == self.win && e.detail == 1 => return true,
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
        false
    }

    fn left_button_down(&self) -> bool {
        let r = self
            .conn
            .query_pointer(self.win)
            .expect("query")
            .reply()
            .expect("reply");
        r.mask.contains(KeyButMask::BUTTON1)
    }
}

struct EngineProc {
    child: Child,
    root: PathBuf,
    socket: PathBuf,
}

fn spawn_engine(display: &str, name: &str) -> EngineProc {
    let root = PathBuf::from(format!("/tmp/pulse-sig{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("cfg")).expect("mkdir");
    std::fs::write(
        root.join("cfg/config.toml"),
        "schema_version = 2\n[clicker]\ninterval_ms = 60000\nhold_ms = 50000\n[target]\nrequire_focus = false\n",
    )
    .expect("config");
    let child = Proc::new(env!("CARGO_BIN_EXE_pulse-engine"))
        .args(["--display", display, "--log-level", "error"])
        .arg("--config-dir")
        .arg(root.join("cfg"))
        .arg("--runtime-dir")
        .arg(root.join("run"))
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn engine");
    let socket = root.join("run/engine.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    EngineProc { child, root, socket }
}

/// Start a clicker whose first press is held for 50 s.
fn hold_left(e: &EngineProc) {
    let mut c = Client::connect(&e.socket).expect("connect");
    c.call(Command::Start).expect("start");
}

fn signal(child: &Child, sig: rustix::process::Signal) {
    let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("pid");
    rustix::process::kill_process(pid, sig).expect("kill");
}

#[test]
fn sigterm_while_held_releases_and_exits_cleanly() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let rx = Receiver::new(&d);
    let mut e = spawn_engine(&d, "term");
    hold_left(&e);
    assert!(rx.wait(true, Duration::from_secs(3)), "press not received");
    assert!(rx.left_button_down());
    signal(&e.child, rustix::process::Signal::TERM);
    assert!(
        rx.wait(false, Duration::from_secs(3)),
        "release not received after SIGTERM"
    );
    assert!(!rx.left_button_down(), "button still logically down");
    let status = e.child.wait().expect("wait");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(!e.socket.exists(), "socket removed on shutdown");
    let _ = std::fs::remove_dir_all(&e.root);
}

#[test]
fn sigkill_while_held_leaves_xtest_state_documented_limitation() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let rx = Receiver::new(&d);
    let mut e = spawn_engine(&d, "kill");
    hold_left(&e);
    assert!(rx.wait(true, Duration::from_secs(3)), "press not received");
    signal(&e.child, rustix::process::Signal::KILL);
    let status = e.child.wait().expect("wait");
    assert_eq!(status.signal(), Some(9));
    // SIGKILL cannot be handled: the X server keeps the XTest button down.
    assert!(!rx.wait(false, Duration::from_millis(500)));
    assert!(
        rx.left_button_down(),
        "expected the documented stuck state after SIGKILL"
    );
    // Recovery = one physical (here: test-injected) release.
    rx.conn
        .xtest_fake_input(5, 1, 0, x11rb::NONE, 0, 0, 0)
        .expect("release");
    rx.conn.sync().expect("sync");
    assert!(!rx.left_button_down());
    // The next engine start cleans the stale socket left by the killed one.
    let e2 = spawn_engine(&d, "kill");
    let mut c = Client::connect(&e2.socket).expect("restarted engine reachable");
    c.call(Command::Shutdown).expect("shutdown");
    let mut child = e2.child;
    child.wait().expect("wait");
    let _ = std::fs::remove_dir_all(&e.root);
}
