//! Stop/cancellation behaviour, settings validation and real-timer accuracy
//! through the engine (mock input backend).

mod common;

use std::time::{Duration, Instant};

use common::{Setup, TestEngine, assert_balanced, clicks};
use pulse_config::Config;
use pulse_core::{BackendKind, EngineState, Key, KeyState, LogicalInput, MouseButton, Pressable, StopReason};
use pulse_protocol::{ClientError, Command, ErrorCode, ResponseData};

fn get_config(e: &TestEngine) -> Config {
    match e.client().call_data(Command::GetConfig).expect("config") {
        Some(ResponseData::Config(c)) => *c,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn stop_releases_a_long_hold_promptly() {
    let e = TestEngine::start(Setup {
        name: "hold",
        config: "schema_version = 2\n[clicker]\ninterval_ms = 30000\nhold_ms = 20000\n[target]\nrequire_focus = false\n",
        ..Setup::default()
    });
    let mut c = e.client();
    c.call(Command::Start).expect("start");
    std::thread::sleep(Duration::from_millis(50));
    let t = Instant::now();
    let r = c.call_data(Command::Stop).expect("stop");
    let elapsed = t.elapsed();
    let Some(ResponseData::Stopped(Some(info))) = r else {
        panic!("unexpected {r:?}")
    };
    assert_eq!(info.reason, StopReason::UserStop);
    assert!(info.release_ok);
    assert!(elapsed < Duration::from_millis(100), "stop took {elapsed:?}");
    assert_eq!(
        e.events(BackendKind::XTest),
        vec![
            LogicalInput::button(MouseButton::Left, KeyState::Pressed),
            LogicalInput::button(MouseButton::Left, KeyState::Released)
        ]
    );
    e.shutdown();
}

#[test]
fn many_start_stop_cycles_leave_nothing_held() {
    let e = TestEngine::start(Setup {
        name: "cycles",
        ..Setup::default()
    });
    let mut c = e.client();
    for _ in 0..1000 {
        c.call(Command::Start).expect("start");
        c.call(Command::Stop).expect("stop");
    }
    let s = e.status();
    assert_eq!(s.state, EngineState::Ready);
    // A stop that reaches the runner before its start executes cancels the
    // run before any event (cancellation generation check), so a few of
    // the 1000 runs may legitimately never begin.
    assert!(
        s.counters.runs <= 1000 && s.counters.runs >= 900,
        "runs {}",
        s.counters.runs
    );
    let ev = e.events(BackendKind::XTest);
    assert_balanced(&ev);
    assert!(clicks(&ev) as u64 <= s.counters.runs, "at most one press per run");
    assert!(clicks(&ev) >= 900, "clicks {}", clicks(&ev));
    e.shutdown();
}

#[test]
fn click_limit_completes_naturally() {
    let e = TestEngine::start(Setup {
        name: "count",
        config: "schema_version = 2\n[clicker]\ninterval_ms = 20\nhold_ms = 5\ncount = 5\n[target]\nrequire_focus = false\n",
        ..Setup::default()
    });
    e.client().call(Command::Start).expect("start");
    let deadline = Instant::now() + Duration::from_secs(2);
    let s = loop {
        let s = e.status();
        if s.last_stop.is_some() || Instant::now() > deadline {
            break s;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(s.state, EngineState::Ready);
    let last = s.last_stop.expect("finished");
    assert_eq!(last.reason, StopReason::Completed);
    assert_eq!(last.clicks, 5);
    let ev = e.events(BackendKind::XTest);
    assert_eq!(clicks(&ev), 5);
    assert_balanced(&ev);
    e.shutdown();
}

#[test]
fn keyboard_key_clicker_presses_the_key() {
    let e = TestEngine::start(Setup {
        name: "key",
        config: "schema_version = 2\n[clicker]\ninput = \"key\"\nkey = \"E\"\ninterval_ms = 50\nhold_ms = 10\ncount = 3\n[target]\nrequire_focus = false\n",
        ..Setup::default()
    });
    let s = e.status();
    assert_eq!(s.clicker.target(), Pressable::Key(Key::E));
    e.client().call(Command::Start).expect("start");
    std::thread::sleep(Duration::from_millis(300));
    let ev = e.events(BackendKind::XTest);
    let e_key = |st| LogicalInput::key(Key::E, st);
    assert_eq!(
        ev,
        vec![
            e_key(KeyState::Pressed),
            e_key(KeyState::Released),
            e_key(KeyState::Pressed),
            e_key(KeyState::Released),
            e_key(KeyState::Pressed),
            e_key(KeyState::Released),
        ]
    );
    assert_eq!(e.status().counters.submitted_clicks, 3, "key presses count as clicks");
    e.shutdown();
}

#[test]
fn real_timer_produces_expected_rate_without_drift() {
    let e = TestEngine::start(Setup {
        name: "rate",
        config: "schema_version = 2\n[clicker]\ninterval_ms = 10\nhold_ms = 3\n[target]\nrequire_focus = false\n",
        ..Setup::default()
    });
    let mut c = e.client();
    c.call(Command::Start).expect("start");
    std::thread::sleep(Duration::from_millis(2_005));
    let Some(ResponseData::Stopped(Some(info))) = c.call_data(Command::Stop).expect("stop") else {
        panic!("no stop info")
    };
    // 2 s at 100 CPS: presses at 0, 10, ..., 2000 ms → about 201.
    assert!((198..=202).contains(&info.clicks), "clicks {}", info.clicks);
    let Some(ResponseData::Diagnostics(d)) = c.call_data(Command::Diagnostics).expect("diag") else {
        panic!("no diagnostics")
    };
    let jitter = d.histograms.iter().find(|h| h.name == "wake_jitter").expect("jitter");
    assert!(jitter.count >= 390, "{}", jitter.count);
    eprintln!(
        "wake jitter over {} wakeups: p50 {:.0} µs, p99 {:.0} µs, max {:.0} µs; missed {}",
        jitter.count, jitter.p50_us, jitter.p99_us, jitter.max_us, info.missed_deadlines
    );
    assert_balanced(&e.events(BackendKind::XTest));
    e.shutdown();
}

#[test]
fn game_window_requirement_blocks_start_with_plain_reasons() {
    let e = TestEngine::start(Setup {
        name: "notarget",
        config: "schema_version = 2\n[target]\nrequire_focus = true\nwindow_class = \"sober\"\n",
        ..Setup::default()
    });
    // No X11 in this harness: the window check cannot work.
    let s = e.status();
    assert!(!s.can_start);
    assert!(s.start_blocker.as_deref().unwrap_or("").contains("needs X11"), "{s:?}");
    let err = e.client().call(Command::Start).expect_err("must fail");
    assert!(
        matches!(err, ClientError::Engine(ref b) if b.code == ErrorCode::InvalidState),
        "{err:?}"
    );
    e.shutdown();

    // With X11 available but no window picked, the reason is "pick it".
    // (Covered end-to-end in x11_engine_tests; here: restriction off works.)
    let e = TestEngine::start(Setup {
        name: "notarget2",
        config: "schema_version = 2\n[target]\nrequire_focus = false\n",
        ..Setup::default()
    });
    assert!(e.status().can_start);
    e.shutdown();
}

#[test]
fn invalid_settings_are_rejected_with_field_paths_and_not_saved() {
    let e = TestEngine::start(Setup {
        name: "invalid",
        ..Setup::default()
    });
    let before = std::fs::read_to_string(e.paths.config_file()).ok();
    let mut cfg = get_config(&e);
    cfg.clicker.interval_ms = 50;
    cfg.clicker.hold_ms = 80;
    let err = e
        .client()
        .call(Command::SetConfig { config: Box::new(cfg) })
        .expect_err("invalid");
    let ClientError::Engine(body) = err else {
        panic!("{err:?}")
    };
    assert_eq!(body.code, ErrorCode::ValidationFailed);
    assert!(
        body.issues.iter().any(|i| i.path == "clicker.hold_ms"),
        "{:?}",
        body.issues
    );
    assert_eq!(
        std::fs::read_to_string(e.paths.config_file()).ok(),
        before,
        "nothing saved"
    );
    e.shutdown();
}

#[test]
fn settings_apply_immediately_but_not_while_clicking() {
    let e = TestEngine::start(Setup {
        name: "apply",
        ..Setup::default()
    });
    let mut cfg = get_config(&e);
    cfg.clicker.set_target(Pressable::Button(MouseButton::Right));
    cfg.clicker.interval_ms = 40;
    cfg.clicker.hold_ms = 10;
    e.client()
        .call(Command::SetConfig {
            config: Box::new(cfg.clone()),
        })
        .expect("apply");
    let s = e.status();
    assert_eq!(s.clicker.target(), Pressable::Button(MouseButton::Right));
    assert_eq!(s.clicker.interval_ms, 40);
    assert!(
        s.warnings.iter().any(|w| w.message.contains("experimental")),
        "sub-100 ms note"
    );
    let saved = std::fs::read_to_string(e.paths.config_file()).expect("saved");
    assert!(saved.contains("mouse_button = \"right\""), "{saved}");

    let mut c = e.client();
    c.call(Command::Start).expect("start");
    let err = c.call(Command::SetConfig { config: Box::new(cfg) }).expect_err("busy");
    assert!(matches!(err, ClientError::Engine(ref b) if b.code == ErrorCode::InvalidState));
    c.call(Command::Stop).expect("stop");
    let ev = e.events(BackendKind::XTest);
    assert!(
        ev.iter()
            .all(|x| x.pressable().0 == Pressable::Button(MouseButton::Right))
    );
    assert_balanced(&ev);
    e.shutdown();
}
