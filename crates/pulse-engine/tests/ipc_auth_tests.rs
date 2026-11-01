//! IPC robustness and security.
//!
//! Not automatable here: rejecting a peer with a *different* UID needs a
//! second account (the `SO_PEERCRED` check in `ipc.rs` drops such peers
//! before reading). Covered manually in docs/security.md.

mod common;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Duration;

use common::{Setup, TestEngine, assert_balanced, fresh_root};
use pulse_core::{BackendKind, EngineState};
use pulse_protocol::{Command, ErrorCode, FrameDecoder, MAX_FRAME, ServerMessage};

fn raw_roundtrip(stream: &mut UnixStream, body: &[u8]) -> Option<ServerMessage> {
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body);
    stream.write_all(&frame).ok()?;
    read_message(stream)
}

fn read_message(stream: &mut UnixStream) -> Option<ServerMessage> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut d = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Ok(Some(f)) = d.next_frame() {
            return serde_json::from_slice(&f).ok();
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => d.push(&buf[..n]),
        }
    }
}

fn error_code(m: Option<ServerMessage>) -> Option<ErrorCode> {
    match m {
        Some(ServerMessage::Response(r)) if !r.ok => r.error.map(|e| e.code),
        _ => None,
    }
}

#[test]
fn version_mismatch_and_malformed_requests_are_rejected_safely() {
    let e = TestEngine::start(Setup {
        name: "proto",
        ..Setup::default()
    });
    let mut s = UnixStream::connect(e.paths.socket()).expect("connect");
    let m = raw_roundtrip(&mut s, br#"{"protocol_version":1,"request_id":"a","command":"status"}"#);
    assert_eq!(error_code(m), Some(ErrorCode::UnsupportedVersion));
    let m = raw_roundtrip(&mut s, b"this is not json");
    assert_eq!(error_code(m), Some(ErrorCode::BadRequest));
    let m = raw_roundtrip(
        &mut s,
        br#"{"protocol_version":2,"request_id":"b","command":"exec","payload":{"cmd":"rm -rf /"}}"#,
    );
    assert_eq!(error_code(m), Some(ErrorCode::BadRequest));
    // The connection survives malformed input and no state was mutated.
    let m = raw_roundtrip(&mut s, br#"{"protocol_version":2,"request_id":"c","command":"hello"}"#);
    assert!(matches!(m, Some(ServerMessage::Response(r)) if r.ok && r.request_id == "c"));
    assert_eq!(e.status().state, EngineState::Ready);
    e.shutdown();
}

#[test]
fn oversized_frame_is_rejected_and_disconnected() {
    let e = TestEngine::start(Setup {
        name: "oversize",
        ..Setup::default()
    });
    let mut s = UnixStream::connect(e.paths.socket()).expect("connect");
    s.write_all(&((MAX_FRAME as u32) + 1).to_be_bytes()).expect("write");
    assert_eq!(error_code(read_message(&mut s)), Some(ErrorCode::BadRequest));
    assert!(read_message(&mut s).is_none(), "connection must be closed");
    // Engine unaffected.
    assert_eq!(e.status().state, EngineState::Ready);
    e.shutdown();
}

#[test]
fn socket_and_directory_permissions_are_private() {
    let e = TestEngine::start(Setup {
        name: "perms",
        ..Setup::default()
    });
    let mode = |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&e.paths.runtime_dir), 0o700);
    assert_eq!(mode(&e.paths.socket()), 0o600);
    assert_eq!(mode(&e.paths.lock_file()), 0o600);
    // Files the engine writes (atomic replace) are private.
    e.client()
        .call(Command::SetTarget {
            rule: pulse_core::TargetRule {
                window_class: "x".into(),
                window_title_pattern: String::new(),
            },
        })
        .expect("save");
    assert_eq!(mode(&e.paths.config_file()), 0o600);
    assert_eq!(mode(&e.paths.config_dir), 0o700);
    e.shutdown();
}

#[test]
fn stale_socket_is_replaced_and_foreign_file_is_refused() {
    let root = fresh_root("stale");
    let run = root.join("run");
    std::fs::create_dir_all(&run).expect("mkdir");
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    // A socket left behind by a crashed engine (no lock held).
    drop(UnixListener::bind(run.join("engine.sock")).expect("bind"));
    std::fs::write(
        root.join("cfg/config.toml"),
        "schema_version = 2\n[target]\nrequire_focus = false\n",
    )
    .expect("cfg");
    let e = TestEngine::start(Setup {
        name: "stale",
        root: Some(root.clone()),
        ..Setup::default()
    });
    assert_eq!(e.status().state, EngineState::Ready);
    let root = e.shutdown_keep();

    // A regular file at the socket path is never deleted.
    std::fs::write(run.join("engine.sock"), b"not a socket").expect("write");
    let err = TestEngine::try_start(Setup {
        name: "stale",
        root: Some(root.clone()),
        ..Setup::default()
    })
    .expect_err("must refuse");
    assert!(err.contains("not a socket"), "{err}");
    assert_eq!(
        std::fs::read(run.join("engine.sock")).expect("still there"),
        b"not a socket"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn second_instance_is_refused() {
    let e = TestEngine::start(Setup {
        name: "single",
        ..Setup::default()
    });
    let err = TestEngine::try_start(Setup {
        name: "single",
        root: Some(e.root.clone()),
        ..Setup::default()
    })
    .expect_err("second engine must fail");
    assert!(err.contains("already running"), "{err}");
    assert_eq!(e.status().state, EngineState::Ready, "first engine unaffected");
    e.shutdown();
}

#[test]
fn connection_churn_and_client_limit() {
    let e = TestEngine::start(Setup {
        name: "churn",
        ..Setup::default()
    });
    for _ in 0..300 {
        let mut c = e.client();
        c.call(Command::Hello).expect("hello");
    }
    // 16 concurrent clients are served; the 17th is closed.
    let mut held: Vec<_> = (0..16).map(|_| e.client()).collect();
    for c in &mut held {
        c.call(Command::Hello).expect("hello");
    }
    let mut extra = UnixStream::connect(e.paths.socket()).expect("connect");
    let m = raw_roundtrip(
        &mut extra,
        br#"{"protocol_version":2,"request_id":"x","command":"hello"}"#,
    );
    assert!(m.is_none(), "17th client must be refused");
    drop(held);
    std::thread::sleep(Duration::from_millis(50));
    e.client().call(Command::Hello).expect("slots freed");
    e.shutdown();
}

#[test]
fn ui_disconnect_does_not_stop_a_running_macro() {
    let e = TestEngine::start(Setup {
        name: "detach",
        ..Setup::default()
    });
    {
        let mut ui = e.client();
        ui.call(Command::Subscribe).expect("subscribe");
        ui.call(Command::Start).expect("start");
    } // "UI" closes.
    std::thread::sleep(Duration::from_millis(250));
    let s = e.status();
    assert_eq!(s.state, EngineState::Running);
    assert!(s.counters.current_run_clicks >= 2);
    e.client().call(Command::Stop).expect("stop");
    assert_balanced(&e.events(BackendKind::XTest));
    e.shutdown();
}

#[test]
fn repeated_stops_are_idempotent() {
    let e = TestEngine::start(Setup {
        name: "stops",
        ..Setup::default()
    });
    let mut c = e.client();
    c.call(Command::Start).expect("start");
    for _ in 0..50 {
        c.call(Command::Stop).expect("stop is always ok");
        c.call(Command::EmergencyStop).expect("emergency stop is always ok");
    }
    let s = e.status();
    assert_eq!(s.state, EngineState::Ready);
    assert_balanced(&e.events(BackendKind::XTest));
    e.shutdown();
}

#[test]
fn subscribers_receive_state_events_not_per_click_traffic() {
    let e = TestEngine::start(Setup {
        name: "events",
        ..Setup::default()
    });
    let mut sub = e.client();
    sub.call(Command::Subscribe).expect("subscribe");
    let mut c = e.client();
    c.call(Command::Start).expect("start");
    std::thread::sleep(Duration::from_millis(550)); // ~6 clicks at 100 ms
    c.call(Command::Stop).expect("stop");
    let mut events = Vec::new();
    while let Ok(Some(ev)) = sub.next_event(Some(Duration::from_millis(200))) {
        events.push(ev);
    }
    // running, stopping, ready — never one message per click.
    assert!((2..=5).contains(&events.len()), "{events:?}");
    e.shutdown();
}
