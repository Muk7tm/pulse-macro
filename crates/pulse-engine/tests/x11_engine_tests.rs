//! End-to-end engine tests on an isolated X server (`PULSE_TEST_DISPLAY`,
//! e.g. Xvfb `:99`): XInput2 hotkeys (keys and mouse side buttons),
//! self-trigger suppression, hold mode, emergency stop and the focus guard.
//! Input injection uses mock backends; physical key and button presses are
//! simulated with XTest, which only these tests (via a test-only engine
//! option) treat as physical.

mod common;

use std::time::Duration;

use common::{Setup, TestEngine, assert_balanced, clicks};
use pulse_core::{BackendKind, EngineState, SideButton, StopReason};
use pulse_protocol::{ClientError, Command, ResponseData};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, PropMode, WindowClass};
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

/// Minimal stand-in for a window manager plus a "physical" keyboard.
struct FakeDesktop {
    conn: RustConnection,
    root: u32,
    target: u32,
    other: u32,
    net_active: u32,
}

impl FakeDesktop {
    fn new(display: &str) -> FakeDesktop {
        let (conn, screen) = x11rb::connect(Some(display)).expect("connect");
        let s = conn.setup().roots[screen].clone();
        let mk = |class: &[u8], title: &[u8]| {
            let w = conn.generate_id().expect("id");
            conn.create_window(
                s.root_depth,
                w,
                s.root,
                0,
                0,
                100,
                100,
                0,
                WindowClass::INPUT_OUTPUT,
                s.root_visual,
                &CreateWindowAux::new(),
            )
            .expect("create");
            conn.change_property8(PropMode::REPLACE, w, AtomEnum::WM_CLASS, AtomEnum::STRING, class)
                .expect("class");
            conn.change_property8(PropMode::REPLACE, w, AtomEnum::WM_NAME, AtomEnum::STRING, title)
                .expect("name");
            w
        };
        let target = mk(b"pulse-target\0PulseTarget\0", b"Target Game");
        let other = mk(b"editor\0Editor\0", b"Some Editor");
        let atom = |n: &str| {
            conn.intern_atom(false, n.as_bytes())
                .expect("atom")
                .reply()
                .expect("atom")
                .atom
        };
        let net_active = atom("_NET_ACTIVE_WINDOW");
        let client_list = atom("_NET_CLIENT_LIST");
        conn.change_property32(
            PropMode::REPLACE,
            s.root,
            client_list,
            AtomEnum::WINDOW,
            &[target, other],
        )
        .expect("client list");
        let d = FakeDesktop {
            root: s.root,
            conn,
            target,
            other,
            net_active,
        };
        d.activate(0);
        d
    }

    fn activate(&self, w: u32) {
        self.conn
            .change_property32(PropMode::REPLACE, self.root, self.net_active, AtomEnum::WINDOW, &[w])
            .expect("set active");
        self.conn.sync().expect("sync");
    }

    fn key(&self, x11_keycode: u8, pressed: bool) {
        let t = if pressed { 2 } else { 3 };
        self.conn
            .xtest_fake_input(t, x11_keycode, 0, x11rb::NONE, 0, 0, 0)
            .expect("fake");
        self.conn.sync().expect("sync");
    }

    fn tap(&self, kc: u8) {
        self.key(kc, true);
        self.key(kc, false);
    }

    fn click(&self, x11_button: u8) {
        for t in [4, 5] {
            self.conn
                .xtest_fake_input(t, x11_button, 0, x11rb::NONE, 0, 0, 0)
                .expect("fake");
        }
        self.conn.sync().expect("sync");
    }
}

impl Drop for FakeDesktop {
    fn drop(&mut self) {
        let _ = self.conn.delete_property(self.root, self.net_active);
        let _ = self.conn.sync();
    }
}

const F6: u8 = 64 + 8;
const F8: u8 = 66 + 8;

fn engine(display: &str, physical: bool, config: &str) -> TestEngine {
    TestEngine::start(Setup {
        name: "x11",
        config,
        display: Some(display),
        xtest_is_physical: physical,
        ..Setup::default()
    })
}

/// F6 toggles, clicks go anywhere. Hotkey and mode are explicit so the tests
/// do not depend on the user-facing defaults.
const UNRESTRICTED: &str = "schema_version = 2\n[clicker]\nactivation_mode = \"toggle\"\nactivation_hotkey = \"F6\"\n[target]\nrequire_focus = false\n";

#[test]
fn pulse_ignores_xtest_sourced_hotkeys_by_default() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let desk = FakeDesktop::new(&d);
    let e = engine(&d, false, UNRESTRICTED);
    desk.tap(F6);
    std::thread::sleep(Duration::from_millis(200));
    let s = e.status();
    assert_eq!(s.state, EngineState::Ready, "synthetic XTest F6 must not trigger Pulse");
    assert_eq!(s.counters.runs, 0);
    e.shutdown();
}

#[test]
fn toggle_hotkey_starts_and_stops_and_records_trigger_latency() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let desk = FakeDesktop::new(&d);
    let e = engine(&d, true, UNRESTRICTED);
    desk.tap(F6);
    e.wait_state(EngineState::Running, Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(250));
    desk.tap(F6);
    let s = e.wait_state(EngineState::Ready, Duration::from_secs(2));
    assert_eq!(s.last_stop.map(|l| l.reason), Some(StopReason::UserStop));
    let ev = e.events(BackendKind::XTest);
    assert!(clicks(&ev) >= 2, "{}", clicks(&ev));
    assert_balanced(&ev);
    let Some(ResponseData::Diagnostics(diag)) = e.client().call_data(Command::Diagnostics).expect("diag") else {
        panic!("no diagnostics")
    };
    let lat = diag
        .histograms
        .iter()
        .find(|h| h.name == "trigger_latency")
        .expect("latency");
    assert_eq!(lat.count, 1);
    eprintln!(
        "trigger latency (hotkey received → first event submitted): {:.1} µs",
        lat.max_us
    );
    e.shutdown();
}

#[test]
fn mouse_side_buttons_work_as_hotkeys() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let desk = FakeDesktop::new(&d);
    let e = engine(
        &d,
        true,
        "schema_version = 2\n[clicker]\nactivation_mode = \"toggle\"\nactivation_hotkey = \"Mouse4\"\nemergency_stop_hotkey = \"Mouse5\"\n[target]\nrequire_focus = false\n",
    );
    desk.click(SideButton::Back.x11_button());
    e.wait_state(EngineState::Running, Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(150));
    desk.click(SideButton::Forward.x11_button());
    let s = e.wait_state(EngineState::Ready, Duration::from_secs(2));
    assert_eq!(s.last_stop.map(|l| l.reason), Some(StopReason::EmergencyStop));
    // Ordinary clicks and the old keyboard hotkey do nothing.
    desk.click(1);
    desk.tap(F6);
    std::thread::sleep(Duration::from_millis(150));
    let s = e.status();
    assert_eq!((s.state, s.counters.runs), (EngineState::Ready, 1));
    assert_balanced(&e.events(BackendKind::XTest));
    e.shutdown();
}

#[test]
fn hold_mode_runs_only_while_held() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let desk = FakeDesktop::new(&d);
    let e = engine(
        &d,
        true,
        "schema_version = 2\n[clicker]\nactivation_mode = \"hold\"\nactivation_hotkey = \"F6\"\n[target]\nrequire_focus = false\n",
    );
    desk.key(F6, true);
    e.wait_state(EngineState::Running, Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(250));
    desk.key(F6, false);
    let s = e.wait_state(EngineState::Ready, Duration::from_secs(2));
    assert_eq!(s.last_stop.map(|l| l.reason), Some(StopReason::TriggerReleased));
    assert_balanced(&e.events(BackendKind::XTest));
    e.shutdown();
}

#[test]
fn emergency_hotkey_stops_immediately() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let desk = FakeDesktop::new(&d);
    let e = engine(&d, true, UNRESTRICTED);
    desk.tap(F6);
    e.wait_state(EngineState::Running, Duration::from_secs(2));
    desk.tap(F8);
    let s = e.wait_state(EngineState::Ready, Duration::from_secs(2));
    assert_eq!(s.last_stop.map(|l| l.reason), Some(StopReason::EmergencyStop));
    // A deliberate F6 press starts clicking again right away.
    desk.tap(F6);
    e.wait_state(EngineState::Running, Duration::from_secs(2));
    desk.tap(F6);
    e.wait_state(EngineState::Ready, Duration::from_secs(2));
    assert_balanced(&e.events(BackendKind::XTest));
    e.shutdown();
}

#[test]
fn focus_guard_blocks_start_and_cancels_on_focus_loss() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let desk = FakeDesktop::new(&d);
    let e = engine(
        &d,
        true,
        "schema_version = 2\n[target]\nrequire_focus = true\nwindow_class = \"PulseTarget\"\n",
    );
    let mut c = e.client();
    // Target not focused: start refused.
    desk.activate(desk.other);
    std::thread::sleep(Duration::from_millis(50));
    let err = c.call(Command::Start).expect_err("must refuse");
    assert!(
        matches!(err, ClientError::Engine(ref b) if b.message.contains("not in front")),
        "{err:?}"
    );

    desk.activate(desk.target);
    std::thread::sleep(Duration::from_millis(50));
    let s = e.status();
    assert!(s.target_focused, "{s:?}");
    assert_eq!(s.active_window.as_ref().map(|w| w.class.as_str()), Some("PulseTarget"));
    c.call(Command::Start).expect("start");
    std::thread::sleep(Duration::from_millis(150));
    desk.activate(desk.other);
    let s = e.wait_state(EngineState::Ready, Duration::from_secs(2));
    assert_eq!(s.last_stop.map(|l| l.reason), Some(StopReason::FocusLost));
    assert_balanced(&e.events(BackendKind::XTest));

    // Window listing via _NET_CLIENT_LIST.
    let Some(ResponseData::Windows(ws)) = c.call_data(Command::ListWindows).expect("list") else {
        panic!("no windows")
    };
    assert!(ws.iter().any(|w| w.class == "PulseTarget" && w.title == "Target Game"));
    assert!(ws.iter().any(|w| w.class == "Editor"));
    e.shutdown();
}

#[test]
fn hotkey_without_a_picked_window_explains_instead_of_clicking() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let Some(d) = display() else { return };
    let desk = FakeDesktop::new(&d);
    let e = engine(
        &d,
        true,
        "schema_version = 2\n[clicker]\nactivation_hotkey = \"F6\"\n[target]\nrequire_focus = true\n",
    );
    let s = e.status();
    assert!(!s.can_start);
    assert_eq!(s.start_blocker.as_deref(), Some("Pick the game window first"));
    desk.tap(F6);
    std::thread::sleep(Duration::from_millis(150));
    let s = e.status();
    assert_eq!(s.state, EngineState::Ready);
    assert_eq!(s.counters.runs, 0);
    assert_eq!(s.last_error.as_deref(), Some("Pick the game window first"));
    assert!(e.events(BackendKind::XTest).is_empty());
    e.shutdown();
}
