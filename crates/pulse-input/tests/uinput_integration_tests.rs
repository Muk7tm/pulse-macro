//! uinput backend integration tests.
//!
//! Run only with `PULSE_TEST_UINPUT=1` and write access to `/dev/uinput`.
//! Every test grabs (EVIOCGRAB) the virtual devices' event nodes before
//! emitting, so injected events are delivered only to the test reader and
//! never to the desktop. Reading the *virtual* nodes needs read access to
//! them (the `input` group on most distributions); the backend itself never
//! reads any `/dev/input/event*` node.

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use evdev::raw_stream::RawDevice;
use evdev::{BusType, EventType, KeyCode, RelativeAxisCode};
use pulse_core::{HeldInputs, Key, KeyState, LogicalInput, MouseButton};
use pulse_input::uinput::{KEYBOARD_NAME, MOUSE_NAME};
use pulse_input::{InputBackend, UinputBackend};

static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn enabled() -> bool {
    let on = std::env::var("PULSE_TEST_UINPUT").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("SKIPPED: set PULSE_TEST_UINPUT=1 (needs write access to /dev/uinput)");
    }
    on
}

struct Grabbed {
    keyboard: RawDevice,
    mouse: RawDevice,
}

fn grab_nodes(nodes: &[PathBuf]) -> Grabbed {
    let mut kbd = None;
    let mut mouse = None;
    for n in nodes {
        // devtmpfs creates the node root-only; udev applies group/ACL
        // permissions shortly after. Retry briefly.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut d = loop {
            match RawDevice::open(n) {
                Ok(d) => break d,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("open {}: {e} (need read access to the virtual node)", n.display()),
            }
        };
        d.grab().expect("EVIOCGRAB");
        match d.name() {
            Some(KEYBOARD_NAME) => kbd = Some(d),
            Some(MOUSE_NAME) => mouse = Some(d),
            other => panic!("unexpected device {other:?}"),
        }
    }
    Grabbed {
        keyboard: kbd.expect("keyboard node"),
        mouse: mouse.expect("mouse node"),
    }
}

/// Read raw `(type, code, value)` triples until `n` arrive or timeout.
fn read_raw(dev: &mut RawDevice, n: usize) -> Vec<(u16, u16, i32)> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut out = Vec::new();
    while out.len() < n && Instant::now() < deadline {
        let timeout = rustix::time::Timespec {
            tv_sec: 0,
            tv_nsec: 100_000_000,
        };
        let ready = {
            let fd = dev.as_fd();
            let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
            rustix::event::poll(&mut fds, Some(&timeout)).expect("poll")
        };
        if ready == 0 {
            continue;
        }
        for ev in dev.fetch_events().expect("read") {
            out.push((ev.event_type().0, ev.code(), ev.value()));
        }
    }
    out
}

const SYN: (u16, u16, i32) = (0, 0, 0);

#[test]
fn devices_have_expected_identity_and_capabilities() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !enabled() {
        return;
    }
    let be = UinputBackend::open().expect("open uinput");
    assert_eq!(be.device_nodes().len(), 2);
    let g = grab_nodes(be.device_nodes());

    assert_eq!(g.keyboard.input_id().bus_type(), BusType::BUS_VIRTUAL);
    let keys = g.keyboard.supported_keys().expect("keys");
    assert!(keys.contains(KeyCode::KEY_A) && keys.contains(KeyCode::KEY_F24));
    assert!(!keys.contains(KeyCode::BTN_LEFT), "keyboard must not advertise buttons");
    assert!(g.keyboard.supported_relative_axes().is_none());
    assert!(
        !g.keyboard.supported_events().contains(EventType::REPEAT),
        "no kernel autorepeat"
    );

    let mkeys = g.mouse.supported_keys().expect("buttons");
    assert!(
        mkeys.contains(KeyCode::BTN_LEFT) && mkeys.contains(KeyCode::BTN_RIGHT) && mkeys.contains(KeyCode::BTN_MIDDLE)
    );
    assert!(!mkeys.contains(KeyCode::KEY_A));
    let rel = g.mouse.supported_relative_axes().expect("rel");
    assert!(rel.contains(RelativeAxisCode::REL_X) && rel.contains(RelativeAxisCode::REL_Y));
}

#[test]
fn events_have_correct_codes_values_and_syn_framing() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !enabled() {
        return;
    }
    let mut be = UinputBackend::open().expect("open uinput");
    let mut g = grab_nodes(be.device_nodes());

    be.emit(&[LogicalInput::button(MouseButton::Left, KeyState::Pressed)])
        .expect("press");
    be.emit(&[LogicalInput::button(MouseButton::Left, KeyState::Released)])
        .expect("release");

    let m = read_raw(&mut g.mouse, 4);
    assert_eq!(m, vec![(1, 0x110, 1), SYN, (1, 0x110, 0), SYN]);

    let w = Key::parse("W").expect("key");
    let shift = Key::parse("LeftShift").expect("key");
    // Two semantically simultaneous key presses share one frame.
    be.emit(&[
        LogicalInput::key(shift, KeyState::Pressed),
        LogicalInput::key(w, KeyState::Pressed),
    ])
    .expect("chord");
    be.emit(&[
        LogicalInput::key(w, KeyState::Released),
        LogicalInput::key(shift, KeyState::Released),
    ])
    .expect("chord up");
    let k = read_raw(&mut g.keyboard, 6);
    assert_eq!(k, vec![(1, 42, 1), (1, 17, 1), SYN, (1, 17, 0), (1, 42, 0), SYN]);
    assert!(be.held().is_empty());
}

#[test]
fn release_all_and_drop_cleanup() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !enabled() {
        return;
    }
    let mut be = UinputBackend::open().expect("open uinput");
    let nodes = be.device_nodes().to_vec();
    let mut g = grab_nodes(&nodes);

    be.emit(&[LogicalInput::button(MouseButton::Right, KeyState::Pressed)])
        .expect("press");
    let mut extra = HeldInputs::new();
    extra.apply_lenient(&LogicalInput::button(MouseButton::Middle, KeyState::Pressed));
    be.release_all(&extra).expect("release all");
    assert!(be.held().is_empty());
    // Right press, then releases for right and middle (middle is a no-op
    // for the kernel because it was never down, so it is filtered).
    let m = read_raw(&mut g.mouse, 4);
    assert_eq!(m, vec![(1, 0x111, 1), SYN, (1, 0x111, 0), SYN]);

    // Held key at drop: the backend releases it before closing the device,
    // and the kernel releases any remaining keys when it unregisters the
    // device. Neither can be read back here: after unregistering, evdev
    // returns ENODEV to readers before draining their buffers.
    be.emit(&[LogicalInput::key(Key::F6, KeyState::Pressed)])
        .expect("press");
    let k = read_raw(&mut g.keyboard, 2);
    assert_eq!(k, vec![(1, 64, 1), SYN]);
    assert!(!be.held().is_empty());
    drop(be);
    drop(g);

    // Devices are destroyed: nodes disappear (udev removes them).
    let deadline = Instant::now() + Duration::from_secs(3);
    while nodes.iter().any(|n| n.exists()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        nodes.iter().all(|n| !n.exists()),
        "virtual devices must not outlive the backend"
    );
}

#[test]
fn devices_are_created_once_and_reused() {
    let _g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    if !enabled() {
        return;
    }
    let mut be = UinputBackend::open().expect("open uinput");
    let nodes_before = be.device_nodes().to_vec();
    let mut g = grab_nodes(&nodes_before);
    // Read in chunks: the kernel's per-reader evdev buffer is small (64+
    // events) and overflows with SYN_DROPPED if a burst is not consumed.
    let mut total = 0;
    for _ in 0..50 {
        for _ in 0..10 {
            be.emit(&[LogicalInput::button(MouseButton::Left, KeyState::Pressed)])
                .expect("press");
            be.emit(&[LogicalInput::button(MouseButton::Left, KeyState::Released)])
                .expect("release");
        }
        let m = read_raw(&mut g.mouse, 40);
        assert_eq!(m.len(), 40);
        assert!(m.chunks(4).all(|c| c == [(1, 0x110, 1), SYN, (1, 0x110, 0), SYN]));
        total += m.len();
    }
    assert_eq!(total, 2000);
    be.check().expect("healthy");
    assert_eq!(be.device_nodes(), nodes_before.as_slice());
}
