//! XTest backend integration tests against a real X server.
//!
//! Run only when `PULSE_TEST_DISPLAY` names an isolated server (e.g. Xvfb
//! `:99`, see `scripts/run_xvfb_integration.sh`). They never default to the
//! live session, because they move the pointer and inject clicks/keys.

use std::time::{Duration, Instant};

use pulse_core::{HeldInputs, Key, KeyState, LogicalInput, MouseButton};
use pulse_input::{InputBackend, KeymapMode, XTestBackend};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{ConnectionExt as _, CreateWindowAux, EventMask, InputFocus, WindowClass};
use x11rb::rust_connection::RustConnection;

/// Tests share one X server and its pointer/focus: run them one at a time.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn test_display() -> Option<String> {
    match std::env::var("PULSE_TEST_DISPLAY") {
        Ok(d) if !d.is_empty() => Some(d),
        _ => {
            eprintln!("SKIPPED: set PULSE_TEST_DISPLAY to an isolated X server (e.g. Xvfb :99)");
            None
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Seen {
    ButtonPress(u8),
    ButtonRelease(u8),
    KeyPress(u8),
    KeyRelease(u8),
    Motion(i16, i16),
}

struct Receiver {
    conn: RustConnection,
    win: u32,
}

impl Receiver {
    fn new(display: &str) -> Receiver {
        let (conn, screen_num) = x11rb::connect(Some(display)).expect("connect receiver");
        let screen = conn.setup().roots[screen_num].clone();
        let win = conn.generate_id().expect("id");
        conn.create_window(
            screen.root_depth,
            win,
            screen.root,
            0,
            0,
            400,
            400,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().override_redirect(1).event_mask(
                EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::POINTER_MOTION,
            ),
        )
        .expect("create");
        conn.map_window(win).expect("map");
        conn.warp_pointer(x11rb::NONE, win, 0, 0, 0, 0, 200, 200).expect("warp");
        conn.set_input_focus(InputFocus::POINTER_ROOT, win, x11rb::CURRENT_TIME)
            .expect("focus");
        conn.get_input_focus().expect("sync").reply().expect("sync reply");
        let r = Receiver { conn, win };
        // Discard events caused by mapping/warping.
        std::thread::sleep(Duration::from_millis(50));
        r.drain();
        r
    }

    fn drain(&self) {
        while let Ok(Some(_)) = self.conn.poll_for_event() {}
    }

    fn collect(&self, n: usize, timeout: Duration) -> Vec<Seen> {
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        while out.len() < n && Instant::now() < deadline {
            match self.conn.poll_for_event().expect("poll") {
                Some(Event::ButtonPress(e)) if e.event == self.win => out.push(Seen::ButtonPress(e.detail)),
                Some(Event::ButtonRelease(e)) if e.event == self.win => out.push(Seen::ButtonRelease(e.detail)),
                Some(Event::KeyPress(e)) if e.event == self.win => out.push(Seen::KeyPress(e.detail)),
                Some(Event::KeyRelease(e)) if e.event == self.win => out.push(Seen::KeyRelease(e.detail)),
                Some(Event::MotionNotify(e)) if e.event == self.win => out.push(Seen::Motion(e.event_x, e.event_y)),
                Some(_) => {}
                None => std::thread::sleep(Duration::from_millis(1)),
            }
        }
        out
    }
}

fn press(b: MouseButton) -> LogicalInput {
    LogicalInput::button(b, KeyState::Pressed)
}

fn release(b: MouseButton) -> LogicalInput {
    LogicalInput::button(b, KeyState::Released)
}

#[test]
fn xtest_delivers_buttons_and_keys() {
    let _g = serial();
    let Some(display) = test_display() else { return };
    let rx = Receiver::new(&display);
    let mut be = XTestBackend::open(Some(&display), false).expect("open xtest");
    assert_eq!(be.keymap().mode(), KeymapMode::Evdev, "Xvfb uses evdev keycodes");
    assert!(be.health().healthy);

    for b in MouseButton::ALL {
        be.emit(&[press(b)]).expect("press");
        be.emit(&[release(b)]).expect("release");
    }
    let a = Key::parse("A").expect("key");
    be.emit(&[LogicalInput::key(a, KeyState::Pressed)]).expect("key down");
    be.emit(&[LogicalInput::key(a, KeyState::Released)]).expect("key up");

    assert!(be.held().is_empty());

    let seen = rx.collect(8, Duration::from_secs(3));
    assert_eq!(
        seen,
        vec![
            Seen::ButtonPress(1),
            Seen::ButtonRelease(1),
            Seen::ButtonPress(3),
            Seen::ButtonRelease(3),
            Seen::ButtonPress(2),
            Seen::ButtonRelease(2),
            Seen::KeyPress(38),
            Seen::KeyRelease(38),
        ]
    );
    be.check().expect("round trip ok");
}

#[test]
fn release_all_releases_backend_and_engine_ledgers() {
    let _g = serial();
    let Some(display) = test_display() else { return };
    let rx = Receiver::new(&display);
    let mut be = XTestBackend::open(Some(&display), false).expect("open xtest");
    be.emit(&[press(MouseButton::Right)]).expect("press");
    let w = Key::parse("W").expect("key");
    let mut engine_ledger = HeldInputs::new();
    engine_ledger.apply_lenient(&LogicalInput::key(w, KeyState::Pressed));
    be.release_all(&engine_ledger).expect("release all");
    assert!(be.held().is_empty());
    let seen = rx.collect(2, Duration::from_secs(3));
    // Release-only for W: Pulse never presses during cleanup; the server
    // ignores the release of a key that is not down, so only the button
    // events are delivered.
    assert_eq!(seen, vec![Seen::ButtonPress(3), Seen::ButtonRelease(3)]);
}

#[test]
fn connection_is_reused_across_many_events() {
    let _g = serial();
    let Some(display) = test_display() else { return };
    let rx = Receiver::new(&display);
    let mut be = XTestBackend::open(Some(&display), false).expect("open xtest");
    // Enough requests to cross the internal sync interval several times.
    for _ in 0..1500 {
        be.emit(&[press(MouseButton::Left)]).expect("press");
        be.emit(&[release(MouseButton::Left)]).expect("release");
    }
    be.check().expect("still healthy");
    let seen = rx.collect(3000, Duration::from_secs(10));
    assert_eq!(seen.len(), 3000);
    assert!(
        seen.chunks(2)
            .all(|c| c == [Seen::ButtonPress(1), Seen::ButtonRelease(1)])
    );
}

#[test]
fn drop_releases_held_inputs() {
    let _g = serial();
    let Some(display) = test_display() else { return };
    let rx = Receiver::new(&display);
    {
        let mut be = XTestBackend::open(Some(&display), false).expect("open xtest");
        be.emit(&[press(MouseButton::Left)]).expect("press");
    }
    let seen = rx.collect(2, Duration::from_secs(3));
    assert_eq!(seen, vec![Seen::ButtonPress(1), Seen::ButtonRelease(1)]);
}

#[test]
fn cannot_connect_error_is_actionable() {
    let err = XTestBackend::open(Some(":12345"), false).expect_err("no such display");
    assert_eq!(err.availability(), pulse_core::BackendAvailability::CannotConnectX11);
    assert!(err.to_string().contains(":12345"));
}

#[test]
fn release_survives_immediate_disconnect() {
    // Regression: the X server may discard unprocessed requests from a
    // client that disconnects, which once left buttons stuck (2 of 20).
    let _g = serial();
    let Some(display) = test_display() else { return };
    let rx = Receiver::new(&display);
    for _ in 0..20 {
        {
            let mut be = XTestBackend::open(Some(&display), false).expect("open xtest");
            be.emit(&[press(MouseButton::Left)]).expect("press");
        }
        let seen = rx.collect(2, Duration::from_secs(3));
        assert_eq!(seen, vec![Seen::ButtonPress(1), Seen::ButtonRelease(1)]);
    }
}
