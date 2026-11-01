//! Backend-switch test matrix with mock backends.

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{Setup, TestEngine, assert_balanced, clicks};
use pulse_core::{BackendKind, EngineState, StopReason};
use pulse_protocol::{ClientError, Command, ErrorCode, ResponseData, SwitchOutcome};

fn switch(e: &TestEngine, backend: BackendKind) -> Result<pulse_protocol::BackendSwitchInfo, ClientError> {
    match e.client().call_data(Command::SelectBackend { backend })? {
        Some(ResponseData::BackendSwitched(i)) => Ok(i),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn xtest_to_uinput_while_idle_switches_and_persists() {
    let e = TestEngine::start(Setup {
        name: "switch",
        ..Setup::default()
    });
    assert_eq!(e.status().active_backend, Some(BackendKind::XTest));
    let info = switch(&e, BackendKind::UInput).expect("switch");
    assert_eq!(info.outcome, SwitchOutcome::Switched);
    let s = e.status();
    assert_eq!(s.active_backend, Some(BackendKind::UInput));
    assert_eq!(s.desired_backend, BackendKind::UInput);
    assert_eq!(s.state, EngineState::Ready);
    assert!(e.backend_dropped(0), "old XTest backend must be closed");
    let cfg = std::fs::read_to_string(e.paths.config_file()).expect("config");
    assert!(cfg.contains("backend = \"uinput\""), "{cfg}");

    // Choice persists across an engine restart.
    let root = e.shutdown_keep();
    let e = TestEngine::start(Setup {
        name: "switch",
        root: Some(root),
        ..Setup::default()
    });
    let s = e.status();
    assert_eq!(s.active_backend, Some(BackendKind::UInput));
    assert_eq!(s.desired_backend, BackendKind::UInput);
    e.shutdown();
}

#[test]
fn same_backend_is_not_reinitialized() {
    let e = TestEngine::start(Setup {
        name: "same",
        ..Setup::default()
    });
    let before = e.opened.load(Ordering::SeqCst);
    let info = switch(&e, BackendKind::XTest).expect("switch");
    assert_eq!(info.outcome, SwitchOutcome::AlreadyActive);
    assert_eq!(e.opened.load(Ordering::SeqCst), before);
    e.shutdown();
}

#[test]
fn switch_is_rejected_while_clicking() {
    let e = TestEngine::start(Setup {
        name: "running",
        ..Setup::default()
    });
    let mut c = e.client();
    c.call(Command::Start).expect("start");
    let err = switch(&e, BackendKind::UInput).expect_err("must be rejected");
    match err {
        ClientError::Engine(b) => {
            assert_eq!(b.code, ErrorCode::InvalidState);
            assert!(
                b.message.contains("Stop clicking to switch the input method"),
                "{}",
                b.message
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(e.status().active_backend, Some(BackendKind::XTest));
    c.call(Command::Stop).expect("stop");
    let ev = e.events(BackendKind::XTest);
    assert!(clicks(&ev) >= 1);
    assert_balanced(&ev);
    e.shutdown();
}

#[test]
fn failed_switch_restores_healthy_previous_backend() {
    let e = TestEngine::start(Setup {
        name: "restore",
        ..Setup::default()
    });
    e.faults.lock().expect("faults").fail_open.push(BackendKind::UInput);
    let err = switch(&e, BackendKind::UInput).expect_err("must fail");
    match err {
        ClientError::Engine(b) => {
            assert_eq!(b.code, ErrorCode::BackendUnavailable);
            assert!(
                b.message.contains("previous input method remains active"),
                "{}",
                b.message
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    let s = e.status();
    assert_eq!(s.active_backend, Some(BackendKind::XTest));
    assert_eq!(
        s.desired_backend,
        BackendKind::XTest,
        "preference only changes on success"
    );
    assert_eq!(s.state, EngineState::Ready);
    assert!(s.can_start, "the hotkey keeps working after a failed switch");
    e.shutdown();
}

#[test]
fn failed_startup_backend_never_reports_ready() {
    let e = TestEngine::start(Setup {
        name: "noinit",
        fail_open: vec![BackendKind::XTest],
        ..Setup::default()
    });
    let s = e.status();
    assert_eq!(s.state, EngineState::Idle);
    assert_eq!(s.active_backend, None);
    assert_eq!(s.desired_backend, BackendKind::XTest);
    assert!(!s.can_start);
    assert!(
        s.start_blocker
            .as_deref()
            .unwrap_or("")
            .contains("No input method is ready"),
        "{s:?}"
    );
    assert!(e.client().call(Command::Start).is_err());
    // Fix the cause, then recover explicitly.
    e.faults.lock().expect("faults").fail_open.clear();
    e.client().call(Command::Reconnect).expect("reconnect");
    let s = e.status();
    assert_eq!(s.state, EngineState::Ready);
    assert!(s.can_start);
    e.shutdown();
}

#[test]
fn failed_switch_back_restores_uinput() {
    let root = common::fresh_root("idle");
    std::fs::write(
        root.join("cfg/config.toml"),
        "schema_version = 2\n[engine]\nbackend = \"uinput\"\n[target]\nrequire_focus = false\n",
    )
    .expect("write");
    let e = TestEngine::start(Setup {
        name: "idle",
        root: Some(root),
        ..Setup::default()
    });
    e.faults.lock().expect("faults").fail_open.push(BackendKind::XTest);
    let err = switch(&e, BackendKind::XTest).expect_err("fails");
    assert!(matches!(err, ClientError::Engine(ref b) if b.code == ErrorCode::BackendUnavailable));
    assert_eq!(e.status().active_backend, Some(BackendKind::UInput), "uinput restored");
    e.shutdown();
}

#[test]
fn mid_run_backend_fault_stops_releases_and_requires_action() {
    let e = TestEngine::start(Setup {
        name: "fault",
        ..Setup::default()
    });
    // Reopen a backend that fails after 3 batches.
    e.faults.lock().expect("faults").fail_after_batches = Some(3);
    switch(&e, BackendKind::UInput).expect("switch");
    let mut c = e.client();
    c.call(Command::Start).expect("start");
    let s = e.wait_state(EngineState::Error, Duration::from_secs(3));
    assert_eq!(s.active_backend, None, "failed backend is closed");
    assert_eq!(s.last_stop.as_ref().map(|l| l.reason), Some(StopReason::BackendFault));
    assert_eq!(s.counters.backend_errors, 1);
    assert!(!s.can_start, "a fault blocks the hotkey until the user acts");
    // Recovery: select an input method again.
    e.faults.lock().expect("faults").fail_after_batches = None;
    switch(&e, BackendKind::XTest).expect("recover");
    assert_eq!(e.status().state, EngineState::Ready);
    e.shutdown();
}

#[test]
fn inactive_backend_receives_no_events() {
    let e = TestEngine::start(Setup {
        name: "inactive",
        ..Setup::default()
    });
    switch(&e, BackendKind::UInput).expect("switch");
    let mut c = e.client();
    c.call(Command::Start).expect("start");
    std::thread::sleep(Duration::from_millis(250));
    c.call(Command::Stop).expect("stop");
    let xtest_events = e.events(BackendKind::XTest);
    assert!(xtest_events.is_empty(), "XTest got {} events", xtest_events.len());
    let uinput_events = e.events(BackendKind::UInput);
    assert!(clicks(&uinput_events) >= 2);
    assert_balanced(&uinput_events);
    e.shutdown();
}
