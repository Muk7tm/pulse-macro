//! The engine control thread.
//!
//! A single `epoll` loop multiplexes the X11 watcher, the IPC socket and its
//! clients, runner notifications and a signal self-pipe. It owns all policy:
//! the state machine, trigger handling, focus guard, transactional backend
//! switching and persistence. It never sits in the per-click path.
//!
//! Whenever an input method is ready and the settings are valid, the hotkey
//! starts the clicker.

use std::collections::VecDeque;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pulse_config::{ClickPlan, Config, Paths};
use pulse_core::hotkeys::{TriggerAction, TriggerConfig, TriggerController};
use pulse_core::{
    ActivationMode, BackendAvailability, BackendKind, EngineState, FocusGuard, HeldInputs, Issue, Nanos, StopReason,
    TargetRule, ValidationFailed,
};
use pulse_input::{BackendOptions, InputBackend, InputError};
use pulse_protocol::{
    BackendInfo, BackendSwitchInfo, Command, DiagnosticsInfo, EngineEvent, ErrorBody, ErrorCode, EventMessage,
    FaultEntry, HelloInfo, PROTOCOL_VERSION, ProcessStats, Request, Response, ResponseData, ServerMessage,
    SessionSummary, StatusInfo, StopInfo, SwitchOutcome,
};
use rustix::event::epoll;
use rustix::io::Errno;
use tracing::{error, info, warn};

use crate::clock::monotonic_now;
use crate::ipc::{self, ClientInput, InstanceLock, IpcServer};
use crate::runner::{Finished, RunnerCmd, RunnerEvent, RunnerHandle, Shared, StartSpec};
use crate::x11_watch::{X11Event, X11Watch};

const TOKEN_SIGNAL: u64 = 1;
const TOKEN_RUNNER: u64 = 2;
const TOKEN_X11: u64 = 3;
const TOKEN_LISTENER: u64 = 4;
const TOKEN_CLIENT_BASE: u64 = 100;

const MAX_FAULTS: usize = 64;
const UINPUT_REGISTRATION_TIMEOUT: Duration = Duration::from_secs(2);

pub type BackendFactory = Box<dyn FnMut(BackendKind) -> Result<Box<dyn InputBackend>, InputError> + Send>;

pub struct EngineOptions {
    pub paths: Paths,
    pub display: Option<String>,
    pub allow_xwayland: bool,
    /// Skip the X11 watcher (tests without an X server): no hotkeys or focus.
    pub disable_x11: bool,
    /// Override backend construction (tests).
    pub backend_factory: Option<BackendFactory>,
    /// Test-only: treat XTEST key events as physical hotkeys. Never set in
    /// production: it disables self-trigger suppression for XTest.
    pub xtest_hotkeys_are_physical: bool,
}

struct ActiveRun {
    id: u64,
    hold_mode: bool,
    stopping: Option<StopReason>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error(transparent)]
    Config(#[from] pulse_config::ConfigError),
    #[error(transparent)]
    Ipc(#[from] ipc::IpcError),
    #[error("system call failed: {0}")]
    Sys(#[from] std::io::Error),
}

impl From<Errno> for StartupError {
    fn from(e: Errno) -> Self {
        StartupError::Sys(e.into())
    }
}

struct Fail {
    code: ErrorCode,
    message: String,
    issues: Vec<Issue>,
}

impl Fail {
    fn new(code: ErrorCode, message: impl Into<String>) -> Fail {
        Fail {
            code,
            message: message.into(),
            issues: Vec::new(),
        }
    }

    fn validation(v: ValidationFailed) -> Fail {
        let message = v
            .issues
            .iter()
            .find(|i| i.is_error())
            .map(|i| i.message.clone())
            .unwrap_or_else(|| v.to_string());
        Fail {
            code: ErrorCode::ValidationFailed,
            message,
            issues: v.issues,
        }
    }
}

type CmdResult = Result<Option<ResponseData>, Fail>;

fn data(d: ResponseData) -> CmdResult {
    Ok(Some(d))
}

pub struct Engine {
    paths: Paths,
    config: Config,
    /// The clicker settings validated into a runnable plan.
    plan: Result<ClickPlan, ValidationFailed>,
    opts_display: Option<String>,
    allow_xwayland: bool,
    disable_x11: bool,
    xtest_hotkeys_are_physical: bool,
    factory: Option<BackendFactory>,
    state: EngineState,
    active_backend: Option<BackendKind>,
    backend_detail: Option<String>,
    runner: RunnerHandle,
    shared: Arc<Shared>,
    x11: Option<X11Watch>,
    x11_error: Option<String>,
    trigger: TriggerController,
    focus: FocusGuard,
    run: Option<ActiveRun>,
    next_run_id: u64,
    last_stop: Option<StopInfo>,
    /// Sticky fault requiring user action (backend switch or reconnect).
    error: Option<String>,
    /// Most recent non-sticky message (e.g. a rejected hotkey start).
    last_message: Option<String>,
    faults: VecDeque<FaultEntry>,
    epoll: std::os::fd::OwnedFd,
    signal_rx: UnixStream,
    ipc: IpcServer,
    _lock: InstanceLock,
    started_at: Nanos,
    shutting_down: bool,
    client_out_registered: Vec<bool>,
}

fn trigger_config(cfg: &Config) -> TriggerConfig {
    TriggerConfig {
        activation: cfg.clicker.activation_hotkey,
        mode: cfg.clicker.activation_mode,
        emergency_stop: cfg.clicker.emergency_stop_hotkey,
    }
}

impl Engine {
    pub fn new(opts: EngineOptions) -> Result<Engine, StartupError> {
        let paths = opts.paths;
        pulse_config::ensure_private_dir(&paths.config_dir)?;
        let config_file = paths.config_file();
        let config = Config::load(&config_file)?;
        if !config_file.exists() {
            config.save(&config_file)?;
            info!(path = %config_file.display(), "wrote default configuration");
        }
        if let Some(e) = config.validate().iter().find(|i| i.is_error()) {
            return Err(StartupError::Config(pulse_config::ConfigError::Invalid(format!(
                "{}: {e}",
                config_file.display()
            ))));
        }

        ipc::prepare_runtime_dir(&paths.runtime_dir)?;
        let lock = ipc::acquire_lock(&paths.lock_file())?;
        let ipc = IpcServer::bind(&paths.socket(), &lock)?;

        let (signal_rx, signal_tx) = UnixStream::pair()?;
        signal_rx.set_nonblocking(true)?;
        for sig in [
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGHUP,
        ] {
            signal_hook::low_level::pipe::register(sig, signal_tx.try_clone()?)?;
        }

        let shared = Arc::new(Shared::default());
        let runner = RunnerHandle::spawn(Arc::clone(&shared))?;
        let ep = epoll::create(epoll::CreateFlags::CLOEXEC)?;
        for (fd, token) in [
            (signal_rx.as_fd(), TOKEN_SIGNAL),
            (runner.events_fd(), TOKEN_RUNNER),
            (ipc.listener_fd(), TOKEN_LISTENER),
        ] {
            epoll::add(&ep, fd, epoll::EventData::new_u64(token), epoll::EventFlags::IN)?;
        }

        let mut engine = Engine {
            paths,
            plan: pulse_config::plan(&config),
            focus: FocusGuard::new(config.target.rule()),
            trigger: TriggerController::new(trigger_config(&config)),
            config,
            opts_display: opts.display,
            allow_xwayland: opts.allow_xwayland,
            disable_x11: opts.disable_x11,
            xtest_hotkeys_are_physical: opts.xtest_hotkeys_are_physical,
            factory: opts.backend_factory,
            state: EngineState::Booting,
            active_backend: None,
            backend_detail: None,
            runner,
            shared,
            x11: None,
            x11_error: None,
            run: None,
            next_run_id: 1,
            last_stop: None,
            error: None,
            last_message: None,
            faults: VecDeque::new(),
            epoll: ep,
            signal_rx,
            ipc,
            _lock: lock,
            started_at: monotonic_now(),
            shutting_down: false,
            client_out_registered: vec![false; ipc::MAX_CLIENTS],
        };
        if let Err(e) = &engine.plan {
            warn!("clicker settings are invalid until fixed: {e}");
        }

        if !engine.disable_x11 {
            engine.connect_x11();
        }
        let desired = engine.config.engine.backend;
        match engine.init_backend(desired) {
            Ok(warnings) => {
                for w in warnings {
                    warn!("{w}");
                }
            }
            Err(e) => {
                warn!(backend = %desired, "backend initialization failed: {e}");
                engine.record_fault(format!("{} initialization failed: {e}", desired.label()));
                engine.last_message = Some(e.to_string());
            }
        }
        engine.recompute_state(None);
        info!(
            socket = %engine.paths.socket().display(),
            state = %engine.state,
            backend = ?engine.active_backend,
            "pulse-engine ready"
        );
        Ok(engine)
    }

    // ------------------------------------------------------------------
    // Event loop
    // ------------------------------------------------------------------

    pub fn run(mut self) -> Result<(), std::io::Error> {
        let mut evs: Vec<epoll::Event> = Vec::with_capacity(32);
        while !self.shutting_down {
            self.drain_x11();
            self.update_client_interest();
            evs.clear();
            match epoll::wait(&self.epoll, rustix::buffer::spare_capacity(&mut evs), None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            for e in evs.iter() {
                let token = e.data.u64();
                let flags = e.flags;
                match token {
                    TOKEN_SIGNAL => {
                        let mut buf = [0u8; 64];
                        let _ = std::io::Read::read(&mut &self.signal_rx, &mut buf);
                        info!("termination signal received");
                        self.begin_shutdown();
                    }
                    TOKEN_RUNNER => self.handle_runner_events(),
                    TOKEN_X11 => self.drain_x11(),
                    TOKEN_LISTENER => self.accept_clients(),
                    t if t >= TOKEN_CLIENT_BASE => {
                        let slot = (t - TOKEN_CLIENT_BASE) as usize;
                        if flags.intersects(epoll::EventFlags::IN | epoll::EventFlags::HUP | epoll::EventFlags::ERR) {
                            self.handle_client(slot);
                        }
                        if flags.contains(epoll::EventFlags::OUT) {
                            self.ipc.flush(slot);
                        }
                    }
                    _ => {}
                }
            }
        }
        self.finish_shutdown();
        Ok(())
    }

    fn update_client_interest(&mut self) {
        for slot in 0..ipc::MAX_CLIENTS {
            let want = self.ipc.has_pending_output(slot);
            if want != self.client_out_registered[slot] {
                if let Some(fd) = self.ipc.client_fd(slot) {
                    let mut flags = epoll::EventFlags::IN;
                    if want {
                        flags |= epoll::EventFlags::OUT;
                    }
                    let _ = epoll::modify(
                        &self.epoll,
                        fd,
                        epoll::EventData::new_u64(TOKEN_CLIENT_BASE + slot as u64),
                        flags,
                    );
                }
                self.client_out_registered[slot] = want;
            }
        }
    }

    fn accept_clients(&mut self) {
        for slot in self.ipc.accept() {
            self.client_out_registered[slot] = false;
            if let Some(fd) = self.ipc.client_fd(slot) {
                let _ = epoll::add(
                    &self.epoll,
                    fd,
                    epoll::EventData::new_u64(TOKEN_CLIENT_BASE + slot as u64),
                    epoll::EventFlags::IN,
                );
            }
        }
    }

    fn handle_client(&mut self, slot: usize) {
        let (inputs, _alive) = self.ipc.read_client(slot);
        for input in inputs {
            let msg = match input {
                ClientInput::Request(req) => {
                    let subscribe = matches!(req.command, Command::Subscribe);
                    let resp = self.handle_request(req);
                    if subscribe && resp.ok {
                        self.ipc.set_subscribed(slot);
                    }
                    resp
                }
                ClientInput::Malformed(m) => self.response(
                    String::new(),
                    Err(Fail::new(ErrorCode::BadRequest, format!("malformed request: {m}"))),
                ),
            };
            if !self.ipc.send(slot, &ServerMessage::Response(msg)) {
                break;
            }
        }
    }

    // ------------------------------------------------------------------
    // State
    // ------------------------------------------------------------------

    fn derived_state(&self) -> EngineState {
        if self.shutting_down {
            EngineState::ShuttingDown
        } else if let Some(run) = &self.run {
            if run.stopping.is_some() {
                EngineState::Stopping
            } else {
                EngineState::Running
            }
        } else if self.error.is_some() {
            EngineState::Error
        } else if self.active_backend.is_none() {
            EngineState::Idle
        } else {
            EngineState::Ready
        }
    }

    fn recompute_state(&mut self, reason: Option<String>) {
        let next = self.derived_state();
        if next != self.state {
            info!(from = %self.state, to = %next, reason = reason.as_deref().unwrap_or(""), "state transition");
            self.state = next;
            let run_id = self.run.as_ref().map(|r| r.id);
            self.broadcast(EngineEvent::StateChanged {
                state: next,
                reason,
                run_id,
            });
        }
    }

    fn broadcast(&mut self, event: EngineEvent) {
        self.ipc.broadcast(&ServerMessage::Event(EventMessage {
            protocol_version: PROTOCOL_VERSION,
            event,
        }));
    }

    fn record_fault(&mut self, message: String) {
        if self.faults.len() == MAX_FAULTS {
            self.faults.pop_front();
        }
        self.faults.push_back(FaultEntry {
            uptime_s: (monotonic_now() - self.started_at) / 1_000_000_000,
            message: message.clone(),
        });
        self.broadcast(EngineEvent::Fault { message });
    }

    fn set_error(&mut self, message: String) {
        error!("{message}");
        self.record_fault(message.clone());
        self.error = Some(message.clone());
        self.recompute_state(Some(message));
    }

    /// Re-derive the plan, hotkeys and target rule from the configuration.
    fn apply_config(&mut self) {
        self.plan = pulse_config::plan(&self.config);
        self.trigger.reconfigure(trigger_config(&self.config));
        self.focus.set_rule(self.config.target.rule());
        self.publish_focus();
    }

    /// Why clicking cannot start right now, ignoring which window is in
    /// front (that is checked when starting). `None` = ready.
    fn start_blocker(&self) -> Option<String> {
        if let Some(e) = &self.error {
            return Some(format!("Problem: {e}"));
        }
        if self.active_backend.is_none() {
            let why = self.last_message.clone().unwrap_or_else(|| "not initialized".into());
            return Some(format!("No input method is ready ({why})"));
        }
        if let Err(e) = &self.plan {
            let first = e.issues.iter().find(|i| i.is_error()).map(|i| i.message.clone());
            return Some(format!("Fix the settings: {}", first.unwrap_or_else(|| e.to_string())));
        }
        if self.config.target.require_focus {
            if self.x11.is_none() {
                return Some(format!(
                    "The game-window check needs X11: {}",
                    self.x11_error.clone().unwrap_or_else(|| "not connected".into())
                ));
            }
            if self.config.target.rule().is_empty() {
                return Some("Pick the game window first".into());
            }
        }
        None
    }

    fn save_config(&self) -> Result<(), Fail> {
        self.config
            .save(&self.paths.config_file())
            .map_err(|e| Fail::new(ErrorCode::Io, e.to_string()))
    }

    // ------------------------------------------------------------------
    // X11
    // ------------------------------------------------------------------

    fn connect_x11(&mut self) {
        match X11Watch::connect(self.opts_display.as_deref(), self.xtest_hotkeys_are_physical) {
            Ok(mut w) => {
                if w.session.xwayland && !self.allow_xwayland {
                    warn!("connected to Xwayland: Pulse officially supports native X11 sessions only");
                }
                let _ = epoll::add(
                    &self.epoll,
                    w.fd(),
                    epoll::EventData::new_u64(TOKEN_X11),
                    epoll::EventFlags::IN,
                );
                let active = w.active_window();
                if !w.has_ewmh_active_window() {
                    warn!("window manager does not publish _NET_ACTIVE_WINDOW; the game window cannot be confirmed");
                }
                self.x11 = Some(w);
                self.x11_error = None;
                self.focus.set_active(active);
                self.publish_focus();
                self.trigger.reset();
            }
            Err(e) => {
                warn!("X11 watcher unavailable: {e}");
                self.x11_error = Some(e);
            }
        }
    }

    fn publish_focus(&self) {
        self.shared
            .focus_ok
            .store(self.focus.target_focused(), Ordering::Release);
    }

    fn drain_x11(&mut self) {
        let Some(w) = self.x11.as_mut() else { return };
        let mut events = Vec::new();
        let result = w.poll_events(&mut events);
        for ev in events {
            self.handle_x11_event(ev);
        }
        if let Err(e) = result {
            self.x11_lost(e);
        }
    }

    fn x11_lost(&mut self, message: String) {
        if let Some(w) = self.x11.take() {
            let _ = epoll::delete(&self.epoll, w.fd());
        }
        self.x11_error = Some(message.clone());
        self.trigger.reset();
        self.focus.set_active(None);
        self.publish_focus();
        if self.run.is_some() {
            self.stop(StopReason::X11ConnectionLost);
        }
        self.set_error(message);
    }

    fn handle_x11_event(&mut self, ev: X11Event) {
        match ev {
            X11Event::Key {
                key,
                pressed,
                received_at,
            } => {
                let running = self.run.as_ref().is_some_and(|r| r.stopping.is_none());
                match self.trigger.on_key(key, pressed, running) {
                    TriggerAction::None => {}
                    TriggerAction::Start => {
                        if let Err(f) = self.start(Some(received_at)) {
                            info!("hotkey start rejected: {}", f.message);
                            self.last_message = Some(f.message.clone());
                            self.broadcast(EngineEvent::Fault { message: f.message });
                        }
                    }
                    TriggerAction::Stop => self.stop(StopReason::UserStop),
                    TriggerAction::Released => {
                        if let Some(run) = self.run.as_ref().filter(|r| r.hold_mode && r.stopping.is_none()) {
                            self.runner.send(RunnerCmd::TriggerReleased { run_id: run.id });
                        }
                    }
                    TriggerAction::EmergencyStop => self.emergency_stop(),
                }
            }
            X11Event::FocusChanged(w) => {
                let changed = self.focus.set_active(w.clone());
                self.publish_focus();
                let focused = self.focus.target_focused();
                let watching = self.config.target.require_focus;
                if watching && !focused && self.run.as_ref().is_some_and(|r| r.stopping.is_none()) {
                    self.stop(StopReason::FocusLost);
                }
                if changed {
                    self.broadcast(EngineEvent::TargetFocus { focused, window: w });
                }
            }
            X11Event::DevicesChanged => {}
        }
    }

    // ------------------------------------------------------------------
    // Backends
    // ------------------------------------------------------------------

    fn construct_backend(&mut self, kind: BackendKind) -> Result<Box<dyn InputBackend>, InputError> {
        if let Some(f) = self.factory.as_mut() {
            return f(kind);
        }
        pulse_input::open_backend(
            kind,
            &BackendOptions {
                display: self.opts_display.clone(),
                allow_xwayland: self.allow_xwayland,
            },
        )
    }

    /// Wait (bounded, initialization only) until the X server lists the
    /// Pulse uinput devices, so the first events are not lost.
    fn wait_for_uinput_registration(&mut self) -> Option<String> {
        if self.factory.is_some() {
            return None;
        }
        let Some(w) = self.x11.as_mut() else {
            return Some("X11 unavailable: cannot confirm that the X server registered the virtual devices".into());
        };
        let deadline = std::time::Instant::now() + UINPUT_REGISTRATION_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if w.has_pulse_uinput_devices() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Some(
            "the X server has not registered the Pulse virtual devices yet; events may not reach X11 applications"
                .into(),
        )
    }

    /// Initialize `kind` when no backend is active. Returns warnings.
    fn init_backend(&mut self, kind: BackendKind) -> Result<Vec<String>, InputError> {
        let backend = self.construct_backend(kind)?;
        let health = backend.health();
        let mut warnings = Vec::new();
        self.runner.install_backend(backend);
        if kind == BackendKind::UInput {
            warnings.extend(self.wait_for_uinput_registration());
        }
        self.active_backend = Some(kind);
        self.backend_detail = Some(health.detail);
        Ok(warnings)
    }

    /// Transactional backend switch.
    fn select_backend(&mut self, kind: BackendKind) -> CmdResult {
        if self.run.is_some() || !self.state.is_safe_for_reconfigure() {
            return Err(Fail::new(
                ErrorCode::InvalidState,
                "Stop clicking to switch the input method",
            ));
        }
        if self.active_backend == Some(kind) {
            if let Some(Ok(h)) = self.runner.check_backend() {
                return data(ResponseData::BackendSwitched(BackendSwitchInfo {
                    outcome: SwitchOutcome::AlreadyActive,
                    active_backend: Some(kind),
                    detail: Some(h.detail),
                    warnings: Vec::new(),
                }));
            }
        }
        // Release with the old backend, then take ownership of it.
        let old = self.runner.take_backend();
        let old_kind = self.active_backend.take();
        self.backend_detail = None;
        match self.construct_backend(kind) {
            Ok(candidate) => {
                let detail = candidate.health().detail;
                self.runner.install_backend(candidate);
                drop(old);
                let mut warnings = Vec::new();
                if kind == BackendKind::UInput {
                    warnings.extend(self.wait_for_uinput_registration());
                }
                self.active_backend = Some(kind);
                self.backend_detail = Some(detail.clone());
                self.error = None;
                self.last_message = None;
                self.config.engine.backend = kind;
                if let Err(f) = self.save_config() {
                    warnings.push(format!("switched, but the choice was not saved: {}", f.message));
                }
                info!(from = ?old_kind, to = %kind, "input backend switched");
                self.recompute_state(Some(format!("backend {kind} active")));
                self.broadcast(EngineEvent::ConfigChanged);
                data(ResponseData::BackendSwitched(BackendSwitchInfo {
                    outcome: SwitchOutcome::Switched,
                    active_backend: Some(kind),
                    detail: Some(detail),
                    warnings,
                }))
            }
            Err(e) => {
                let message = format!("{} could not be initialized: {e}", kind.label());
                self.record_fault(message.clone());
                // Roll back only to a backend that still passes a health check.
                let mut restored = false;
                if let (Some(mut old), Some(old_kind)) = (old, old_kind) {
                    if old.check().is_ok() {
                        self.backend_detail = Some(old.health().detail);
                        self.runner.install_backend(old);
                        self.active_backend = Some(old_kind);
                        restored = true;
                    }
                }
                if restored {
                    self.recompute_state(None);
                } else {
                    self.set_error(message.clone());
                }
                Err(Fail::new(
                    ErrorCode::BackendUnavailable,
                    if restored {
                        format!("{message}. The previous input method remains active.")
                    } else {
                        format!("{message}. No input method is active.")
                    },
                ))
            }
        }
    }

    fn backend_infos(&mut self) -> Vec<BackendInfo> {
        let desired = self.config.engine.backend;
        BackendKind::ALL
            .iter()
            .map(|&kind| {
                let active = self.active_backend == Some(kind);
                let (availability, detail, remediation) = if active {
                    (
                        BackendAvailability::Ready,
                        self.backend_detail.clone().unwrap_or_default(),
                        None,
                    )
                } else if self.factory.is_some() {
                    (BackendAvailability::Unknown, "test backend factory".to_owned(), None)
                } else {
                    let r = match kind {
                        BackendKind::XTest => pulse_input::probe_xtest(self.opts_display.as_deref()),
                        BackendKind::UInput => pulse_input::probe_uinput(),
                    };
                    (r.availability, r.detail, r.remediation)
                };
                BackendInfo {
                    kind,
                    label: kind.label().to_owned(),
                    description: match kind {
                        BackendKind::XTest => {
                            "Sends clicks through the X11 XTEST extension. Recommended; needs no device permissions."
                        }
                        BackendKind::UInput => {
                            "Creates a Linux virtual keyboard and mouse. Try it if a game ignores the standard method; needs /dev/uinput access."
                        }
                    }
                    .to_owned(),
                    availability,
                    detail,
                    remediation,
                    desired: desired == kind,
                    active,
                }
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Runs
    // ------------------------------------------------------------------

    fn start(&mut self, trigger_at: Option<Nanos>) -> Result<u64, Fail> {
        if self.run.is_some() {
            return Err(Fail::new(ErrorCode::InvalidState, "Already clicking"));
        }
        if let Some(blocker) = self.start_blocker() {
            return Err(Fail::new(ErrorCode::InvalidState, blocker));
        }
        let Ok(plan) = &self.plan else {
            return Err(Fail::new(ErrorCode::InvalidState, "Fix the settings first"));
        };
        if plan.require_focus && !self.focus.target_focused() {
            return Err(Fail::new(
                ErrorCode::InvalidState,
                "The game window is not in front: switch to it and press the hotkey",
            ));
        }
        let program = plan.program;
        let hold_mode = plan.activation_mode == ActivationMode::Hold;
        let require_focus = plan.require_focus;
        let mut needed = HeldInputs::new();
        needed.apply_lenient(&program.target.press());
        match self.runner.supports(needed) {
            Some(Ok(())) => {}
            Some(Err(e)) => return Err(Fail::new(ErrorCode::ValidationFailed, e.to_string())),
            None => {
                return Err(Fail::new(
                    ErrorCode::BackendUnavailable,
                    "the input method did not respond",
                ));
            }
        }
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        let spec = StartSpec {
            run_id,
            program,
            hold_mode,
            require_focus,
            stop_on_suspend: self.config.engine.stop_on_suspend,
            trigger_at,
        };
        if !self.runner.send(RunnerCmd::Start(spec)) {
            self.set_error("the runner thread is not running".into());
            return Err(Fail::new(ErrorCode::Internal, "runner thread unavailable"));
        }
        self.run = Some(ActiveRun {
            id: run_id,
            hold_mode,
            stopping: None,
        });
        self.last_message = None;
        self.recompute_state(Some(format!("run {run_id} started")));
        Ok(run_id)
    }

    fn stop(&mut self, reason: StopReason) {
        if let Some(run) = self.run.as_mut() {
            if run.stopping.is_none() {
                run.stopping = Some(reason);
                let id = run.id;
                self.runner.cancel(id, reason);
                self.recompute_state(Some(reason.to_string()));
            }
        }
    }

    fn emergency_stop(&mut self) {
        info!("emergency stop");
        self.stop(StopReason::EmergencyStop);
    }

    /// Wait (bounded) for the active run to finish after a stop request.
    fn wait_for_finish(&mut self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        while self.run.is_some() && std::time::Instant::now() < deadline {
            self.handle_runner_events();
            if self.run.is_some() {
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    }

    fn handle_runner_events(&mut self) {
        for ev in self.runner.drain_events() {
            match ev {
                RunnerEvent::Started { .. } => {}
                RunnerEvent::StartRejected { run_id, message } => {
                    if self.run.as_ref().is_some_and(|r| r.id == run_id) {
                        self.run = None;
                    }
                    self.last_message = Some(message.clone());
                    self.record_fault(message.clone());
                    self.recompute_state(Some(message));
                }
                RunnerEvent::Finished(f) => self.run_finished(f),
                RunnerEvent::BackendFault { message } => {
                    self.record_fault(format!("input backend fault: {message}"));
                }
            }
        }
        if !self.runner.is_alive() && !self.shutting_down && self.error.is_none() {
            self.active_backend = None;
            self.set_error("the runner thread stopped unexpectedly".into());
        }
    }

    fn run_finished(&mut self, f: Finished) {
        let Some(run) = self.run.take_if(|r| r.id == f.run_id) else {
            return;
        };
        // A requested stop reason takes precedence over what the runner saw.
        let reason = match (run.stopping, f.reason) {
            (Some(req), StopReason::UserStop) => req,
            (_, r) => r,
        };
        let info = StopInfo {
            run_id: f.run_id,
            reason,
            clicks: f.stats.clicks,
            events: f.stats.events,
            missed_deadlines: f.stats.missed_deadlines,
            duration_ms: f.duration / 1_000_000,
            release_ok: f.release_ok,
        };
        info!(
            run = f.run_id,
            reason = %reason,
            clicks = info.clicks,
            missed = info.missed_deadlines,
            ms = info.duration_ms,
            "run finished"
        );
        self.last_stop = Some(info);
        if reason == StopReason::BackendFault {
            drop(self.runner.take_backend());
            self.active_backend = None;
            self.backend_detail = None;
            self.set_error(format!(
                "{reason}: the input method was closed; select one again to recover"
            ));
        }
        if !f.release_ok {
            self.set_error("Releasing held input failed: a key or button may still be logically pressed. Press it once physically and check the input method.".into());
        }
        self.recompute_state(Some(reason.to_string()));
    }

    // ------------------------------------------------------------------
    // Requests
    // ------------------------------------------------------------------

    fn response(&self, request_id: String, result: CmdResult) -> Response {
        match result {
            Ok(data) => Response {
                protocol_version: PROTOCOL_VERSION,
                request_id,
                ok: true,
                state: self.state,
                active_backend: self.active_backend,
                error: None,
                data,
            },
            Err(f) => Response {
                protocol_version: PROTOCOL_VERSION,
                request_id,
                ok: false,
                state: self.state,
                active_backend: self.active_backend,
                error: Some(ErrorBody {
                    code: f.code,
                    message: f.message,
                    issues: f.issues,
                }),
                data: None,
            },
        }
    }

    pub fn handle_request(&mut self, req: Request) -> Response {
        let id = req.request_id.clone();
        if req.protocol_version != PROTOCOL_VERSION {
            return self.response(
                id,
                Err(Fail::new(
                    ErrorCode::UnsupportedVersion,
                    format!(
                        "protocol version {} is not supported (engine speaks {PROTOCOL_VERSION}); update or restart Pulse",
                        req.protocol_version
                    ),
                )),
            );
        }
        if id.len() > 64 {
            return self.response(
                String::new(),
                Err(Fail::new(ErrorCode::BadRequest, "request_id too long")),
            );
        }
        let result = self.dispatch(req.command);
        self.response(id, result)
    }

    fn dispatch(&mut self, cmd: Command) -> CmdResult {
        match cmd {
            Command::Hello => data(ResponseData::Hello(HelloInfo {
                engine_version: env!("CARGO_PKG_VERSION").to_owned(),
                protocol_version: PROTOCOL_VERSION,
                pid: std::process::id(),
            })),
            Command::Status => data(ResponseData::Status(Box::new(self.status()))),
            Command::ListBackends => data(ResponseData::Backends(self.backend_infos())),
            Command::SelectBackend { backend } => self.select_backend(backend),
            Command::GetConfig => data(ResponseData::Config(Box::new(self.config.clone()))),
            Command::SetConfig { config } => self.set_config(*config),
            Command::Start => self.start(None).map(|run_id| Some(ResponseData::Started { run_id })),
            Command::Stop => {
                if self.run.is_none() {
                    return data(ResponseData::Stopped(None));
                }
                self.stop(StopReason::UserStop);
                self.wait_for_finish(Duration::from_secs(1));
                data(ResponseData::Stopped(self.last_stop.clone()))
            }
            Command::EmergencyStop => {
                self.emergency_stop();
                data(ResponseData::Ack)
            }
            Command::SetTarget { rule } => self.set_target(rule),
            Command::ActiveWindow => {
                let w = self
                    .x11
                    .as_mut()
                    .ok_or_else(|| Fail::new(ErrorCode::X11Unavailable, "no X11 connection"))?
                    .active_window();
                data(ResponseData::Window(w))
            }
            Command::ListWindows => {
                let w = self
                    .x11
                    .as_ref()
                    .ok_or_else(|| Fail::new(ErrorCode::X11Unavailable, "no X11 connection"))?
                    .list_windows();
                data(ResponseData::Windows(w))
            }
            Command::Diagnostics => data(ResponseData::Diagnostics(Box::new(self.diagnostics()))),
            Command::ResetDiagnostics => {
                self.shared.metrics.reset();
                self.faults.clear();
                data(ResponseData::Ack)
            }
            Command::Subscribe => data(ResponseData::Ack),
            Command::Reconnect => self.reconnect(),
            Command::Shutdown => {
                self.begin_shutdown();
                data(ResponseData::Ack)
            }
        }
    }

    fn reconnect(&mut self) -> CmdResult {
        if self.run.is_some() {
            return Err(Fail::new(ErrorCode::InvalidState, "Stop clicking first"));
        }
        if self.x11.is_none() && !self.disable_x11 {
            self.connect_x11();
            if let Some(e) = &self.x11_error {
                return Err(Fail::new(ErrorCode::X11Unavailable, e.clone()));
            }
        }
        if self.active_backend.is_none() {
            let kind = self.config.engine.backend;
            if let Err(e) = self.init_backend(kind) {
                return Err(Fail::new(ErrorCode::BackendUnavailable, e.to_string()));
            }
        } else if let Some(Err(e)) = self.runner.check_backend() {
            return Err(Fail::new(ErrorCode::BackendUnavailable, e.to_string()));
        }
        self.error = None;
        self.last_message = None;
        self.recompute_state(Some("reconnected".into()));
        data(ResponseData::Ack)
    }

    fn set_config(&mut self, mut new: Config) -> CmdResult {
        if self.run.is_some() {
            return Err(Fail::new(ErrorCode::InvalidState, "Stop clicking to change settings"));
        }
        // Input-method changes go through select_backend's transaction only.
        new.engine.backend = self.config.engine.backend;
        pulse_config::plan(&new).map_err(Fail::validation)?;
        new.save(&self.paths.config_file())
            .map_err(|e| Fail::new(ErrorCode::Io, e.to_string()))?;
        self.config = new;
        self.apply_config();
        self.last_message = None;
        self.recompute_state(None);
        self.broadcast(EngineEvent::ConfigChanged);
        data(ResponseData::Config(Box::new(self.config.clone())))
    }

    fn set_target(&mut self, rule: TargetRule) -> CmdResult {
        if self.run.is_some() {
            return Err(Fail::new(
                ErrorCode::InvalidState,
                "Stop clicking to change the game window",
            ));
        }
        rule.validate().map_err(|e| Fail::new(ErrorCode::ValidationFailed, e))?;
        let previous = self.config.target.clone();
        self.config.target.set_rule(&rule);
        if let Err(f) = self.save_config() {
            self.config.target = previous;
            return Err(f);
        }
        self.apply_config();
        self.broadcast(EngineEvent::ConfigChanged);
        data(ResponseData::Ack)
    }

    fn session(&self) -> SessionSummary {
        match &self.x11 {
            Some(w) => w.session.clone(),
            None => SessionSummary {
                display: self.opts_display.clone().or_else(|| std::env::var("DISPLAY").ok()),
                xdg_session_type: std::env::var("XDG_SESSION_TYPE").ok(),
                ..SessionSummary::default()
            },
        }
    }

    fn status(&self) -> StatusInfo {
        let blocker = self.start_blocker();
        StatusInfo {
            engine_version: env!("CARGO_PKG_VERSION").to_owned(),
            state: self.state,
            desired_backend: self.config.engine.backend,
            active_backend: self.active_backend,
            backend_detail: self.backend_detail.clone(),
            clicker: self.config.clicker.clone(),
            can_start: blocker.is_none(),
            start_blocker: blocker,
            warnings: self.plan.as_ref().map(|p| p.warnings.clone()).unwrap_or_default(),
            run_id: self.run.as_ref().map(|r| r.id),
            last_stop: self.last_stop.clone(),
            last_error: self.error.clone().or_else(|| self.last_message.clone()),
            require_focus: self.config.target.require_focus,
            target: self.config.target.rule(),
            target_focused: self.focus.target_focused(),
            active_window: self.focus.active().cloned(),
            hotkeys_available: self.x11.is_some(),
            counters: self.shared.metrics.counters(),
            session: self.session(),
            uptime_s: (monotonic_now() - self.started_at) as f64 / 1e9,
        }
    }

    fn diagnostics(&mut self) -> DiagnosticsInfo {
        DiagnosticsInfo {
            histograms: self.shared.metrics.summaries(),
            faults: self.faults.iter().cloned().collect(),
            process: process_stats(),
            session: self.session(),
            backends: self.backend_infos(),
            keymap: self.x11.as_ref().map(|w| w.keymap_description()),
            synthetic_devices: self.x11.as_ref().map(|w| w.synthetic_devices()).unwrap_or_default(),
        }
    }

    // ------------------------------------------------------------------
    // Shutdown
    // ------------------------------------------------------------------

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.stop(StopReason::Shutdown);
        self.shutting_down = true;
        self.recompute_state(Some("shutdown requested".into()));
    }

    fn finish_shutdown(&mut self) {
        self.wait_for_finish(Duration::from_millis(500));
        // Flush the final responses/events to clients.
        for slot in 0..ipc::MAX_CLIENTS {
            self.ipc.flush(slot);
        }
        self.runner.shutdown();
        info!("pulse-engine stopped");
    }

    pub fn state(&self) -> EngineState {
        self.state
    }
}

fn read_proc(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// On-demand process statistics from procfs (never sampled periodically).
pub fn process_stats() -> ProcessStats {
    let mut s = ProcessStats {
        pid: std::process::id(),
        ..ProcessStats::default()
    };
    let status = read_proc("/proc/self/status");
    let field = |name: &str| -> u64 {
        status
            .lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    s.rss_kib = field("VmRSS:");
    s.threads = field("Threads:");
    // Context switches are per thread: sum over all engine threads.
    if let Ok(tasks) = std::fs::read_dir("/proc/self/task") {
        for task in tasks.flatten() {
            let st = std::fs::read_to_string(task.path().join("status")).unwrap_or_default();
            for l in st.lines() {
                let v = |p: &str| l.strip_prefix(p).and_then(|v| v.trim().parse::<u64>().ok());
                if let Some(n) = v("voluntary_ctxt_switches:") {
                    s.voluntary_ctx_switches += n;
                } else if let Some(n) = v("nonvoluntary_ctxt_switches:") {
                    s.involuntary_ctx_switches += n;
                }
            }
        }
    }
    let stat = read_proc("/proc/self/stat");
    // Fields after the parenthesized command name; utime/stime are 14/15.
    if let Some(rest) = stat.rfind(')').map(|i| &stat[i + 2..]) {
        let f: Vec<&str> = rest.split_whitespace().collect();
        let ticks = rustix::param::clock_ticks_per_second() as f64;
        if f.len() > 12 && ticks > 0.0 {
            s.cpu_user_s = f[11].parse::<f64>().unwrap_or(0.0) / ticks;
            s.cpu_system_s = f[12].parse::<f64>().unwrap_or(0.0) / ticks;
        }
    }
    s
}
