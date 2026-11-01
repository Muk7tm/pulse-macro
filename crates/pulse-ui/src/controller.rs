//! UI-thread controller: owns the view state, renders it into the Slint
//! globals and turns user actions into queued IPC jobs. It never blocks:
//! every engine interaction goes through the worker threads.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use pulse_config::{Config, InputKind};
use pulse_core::{ActivationMode, BackendAvailability, BackendKind, EngineState, Key, TargetRule};
use pulse_protocol::{BackendInfo, Command, EngineEvent, Response, ResponseData, StatusInfo};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::form::{self, Field, FormValues, Messages};
use crate::launcher::{self, EngineLocation};
use crate::worker::{Job, Tag, UiMsg};
use crate::{Actions, Form, MainWindow, Tone, Ui};

thread_local! {
    static CTL: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

pub fn install(c: Controller) {
    CTL.with(|cell| *cell.borrow_mut() = Some(c));
}

/// Run `f` with the controller. If it is already borrowed (re-entrant
/// callback), the call is deferred to the next event-loop iteration.
pub fn with<F: FnOnce(&mut Controller) + 'static>(f: F) {
    let deferred = CTL.with(|cell| match cell.try_borrow_mut() {
        Ok(mut guard) => {
            if let Some(c) = guard.as_mut() {
                f(c);
            }
            None
        }
        Err(_) => Some(f),
    });
    if let Some(f) = deferred {
        Timer::single_shot(Duration::ZERO, move || with(f));
    }
}

pub fn dispatch(msg: UiMsg) {
    with(move |c| c.handle(msg));
}

/// How long to wait for a freshly started engine before reporting failure.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an older engine may take to exit after SIGTERM.
const RESTART_TIMEOUT: Duration = Duration::from_secs(5);
/// Settings are applied this long after the last edit.
const APPLY_DELAY: Duration = Duration::from_millis(350);
/// Live click counter refresh while clicking (2 Hz).
const POLL_INTERVAL: Duration = Duration::from_millis(500);

const BACKENDS: [BackendKind; 2] = [BackendKind::XTest, BackendKind::UInput];
const BACKEND_LABELS: [&str; 2] = ["Standard (X11)", "Virtual device (uinput)"];

/// The connection to the background engine.
#[derive(Clone, Debug, PartialEq)]
enum Link {
    Connecting,
    Starting {
        since: Instant,
    },
    StartFailed(String),
    /// The engine went away after being connected.
    Stopped,
    /// An engine speaking another protocol version is running.
    Older(Option<u32>),
    Restarting {
        since: Instant,
    },
    Online,
}

pub struct Controller {
    ui: slint::Weak<MainWindow>,
    jobs: Sender<Job>,
    visible: Arc<AtomicBool>,
    loc: EngineLocation,
    link: Link,
    started_engine: bool,
    status: Option<StatusInfo>,
    config: Option<Config>,
    backends: Vec<BackendInfo>,
    /// The user edited the form and the change is not yet confirmed.
    edit_pending: bool,
    save_in_flight: bool,
    status_in_flight: bool,
    status_again: bool,
    backend_busy: bool,
    countdown: u32,
    apply_timer: Timer,
    poll_timer: Timer,
    house_timer: Timer,
    countdown_timer: Timer,
    toast_timer: Timer,
    link_timer: Timer,
    quit_timer: Timer,
}

/// Position of a key name in the key list shown by the key fields.
fn key_index(name: &str) -> i32 {
    Key::all()
        .position(|k| k.name() == name)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(0)
}

fn tone_for(availability: BackendAvailability) -> Tone {
    match availability {
        BackendAvailability::Ready => Tone::Success,
        BackendAvailability::Unknown => Tone::Neutral,
        _ => Tone::Error,
    }
}

impl Controller {
    pub fn new(ui: &MainWindow, jobs: Sender<Job>, visible: Arc<AtomicBool>, loc: EngineLocation) -> Controller {
        let names: Vec<SharedString> = crate::keys::all_key_names();
        let form = ui.global::<Form>();
        form.set_key_names(ModelRc::new(VecModel::from(names)));
        form.set_backend_names(ModelRc::new(VecModel::from(
            BACKEND_LABELS
                .iter()
                .map(|s| SharedString::from(*s))
                .collect::<Vec<_>>(),
        )));
        ui.global::<Ui>().set_version(env!("CARGO_PKG_VERSION").into());
        let mut c = Controller {
            ui: ui.as_weak(),
            jobs,
            visible,
            loc,
            link: Link::Connecting,
            started_engine: false,
            status: None,
            config: None,
            backends: Vec::new(),
            edit_pending: false,
            save_in_flight: false,
            status_in_flight: false,
            status_again: false,
            backend_busy: false,
            countdown: 0,
            apply_timer: Timer::default(),
            poll_timer: Timer::default(),
            house_timer: Timer::default(),
            countdown_timer: Timer::default(),
            toast_timer: Timer::default(),
            link_timer: Timer::default(),
            quit_timer: Timer::default(),
        };
        c.start_housekeeping();
        c.render();
        c
    }

    fn ui(&self) -> Option<MainWindow> {
        self.ui.upgrade()
    }

    fn send(&self, command: Command, tag: Tag) {
        let _ = self.jobs.send(Job { command, tag });
    }

    fn online(&self) -> bool {
        self.link == Link::Online
    }

    fn running(&self) -> bool {
        self.online()
            && self
                .status
                .as_ref()
                .is_some_and(|s| matches!(s.state, EngineState::Running | EngineState::Stopping))
    }

    fn window_visible(&self) -> bool {
        self.ui()
            .is_some_and(|ui| ui.window().is_visible() && !ui.window().is_minimized())
    }

    fn toast(&mut self, text: impl Into<String>, tone: Tone) {
        let Some(ui) = self.ui() else { return };
        let g = ui.global::<Ui>();
        g.set_toast(text.into().into());
        g.set_toast_tone(tone);
        let weak = self.ui.clone();
        self.toast_timer
            .start(TimerMode::SingleShot, Duration::from_secs(5), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<Ui>().set_toast(SharedString::new());
                }
            });
    }

    /// While disconnected: refresh the visibility flag the event watcher
    /// uses to decide whether to retry (1 s cadence, only then).
    fn start_housekeeping(&mut self) {
        let visible = Arc::clone(&self.visible);
        let weak = self.ui.clone();
        self.house_timer
            .start(TimerMode::Repeated, Duration::from_secs(1), move || {
                let v = weak
                    .upgrade()
                    .is_some_and(|ui| ui.window().is_visible() && !ui.window().is_minimized());
                visible.store(v, Ordering::Relaxed);
            });
    }

    /// While clicking: refresh the counter at 2 Hz if the window is visible.
    fn update_poll_timer(&mut self) {
        if self.running() {
            if !self.poll_timer.running() {
                self.poll_timer.start(TimerMode::Repeated, POLL_INTERVAL, || {
                    with(|c| {
                        if c.window_visible() {
                            c.request_status();
                        }
                    });
                });
            }
        } else {
            self.poll_timer.stop();
        }
    }

    fn request_status(&mut self) {
        if self.status_in_flight {
            self.status_again = true;
        } else {
            self.status_in_flight = true;
            self.send(Command::Status, Tag::Status);
        }
    }

    fn refresh_all(&mut self) {
        self.request_status();
        self.send(Command::GetConfig, Tag::Config);
        self.send(Command::ListBackends, Tag::Backends);
    }

    // ------------------------------------------------------------------
    // Incoming messages
    // ------------------------------------------------------------------

    pub fn handle(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::Connection {
                connected,
                not_running,
                message,
            } => self.connection(connected, not_running, message),
            UiMsg::Mismatch { version } => {
                if !matches!(self.link, Link::Restarting { .. }) {
                    self.link = Link::Older(version);
                    self.render();
                }
            }
            UiMsg::Event(ev) => self.event(ev),
            UiMsg::Reply { tag, result } => self.reply(tag, result),
        }
    }

    fn connection(&mut self, connected: bool, not_running: bool, message: String) {
        if crate::worker::trace_enabled() {
            eprintln!("[ui-trace] connected={connected} not_running={not_running} {message}");
        }
        if connected {
            self.link = Link::Online;
            self.link_timer.stop();
            self.house_timer.stop();
            self.refresh_all();
            self.render();
            return;
        }
        self.start_housekeeping();
        self.poll_timer.stop();
        match &self.link {
            // Waiting for an engine we started (or restarted).
            Link::Starting { since } if since.elapsed() < START_TIMEOUT => {}
            Link::Restarting { .. } => {}
            Link::Connecting if not_running && !self.started_engine => self.start_engine(),
            Link::Connecting if !not_running => self.link = Link::StartFailed(message),
            Link::Online => self.link = Link::Stopped,
            _ => {}
        }
        self.render();
    }

    fn start_engine(&mut self) {
        self.started_engine = true;
        match launcher::spawn_engine(&self.loc) {
            Ok(()) => {
                self.link = Link::Starting { since: Instant::now() };
                self.visible.store(true, Ordering::Relaxed);
                self.link_timer.start(TimerMode::SingleShot, START_TIMEOUT, || {
                    with(|c| {
                        if matches!(c.link, Link::Starting { .. }) {
                            c.link = Link::StartFailed(
                                "Pulse did not start. Details are in engine.log in Pulse's state folder.".into(),
                            );
                            c.render();
                        }
                    });
                });
            }
            Err(e) => self.link = Link::StartFailed(e),
        }
        self.render();
    }

    fn restart_older_engine(&mut self) {
        let version = match self.link {
            Link::Older(v) => v,
            _ => None,
        };
        if let Err(e) = launcher::stop_old_engine(&self.loc, version) {
            self.link = Link::StartFailed(format!("Could not stop the older Pulse: {e}"));
            self.render();
            return;
        }
        self.link = Link::Restarting { since: Instant::now() };
        self.render();
        self.link_timer
            .start(TimerMode::Repeated, Duration::from_millis(100), || {
                with(|c| c.restart_tick());
            });
    }

    fn restart_tick(&mut self) {
        let Link::Restarting { since } = self.link else {
            self.link_timer.stop();
            return;
        };
        if !self.loc.socket.exists() {
            self.link_timer.stop();
            self.start_engine();
        } else if since.elapsed() > RESTART_TIMEOUT {
            self.link_timer.stop();
            self.link = Link::StartFailed("The older Pulse did not exit. Log out and back in, then try again.".into());
            self.render();
        }
    }

    fn event(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::StateChanged { .. } | EngineEvent::TargetFocus { .. } => self.request_status(),
            EngineEvent::Fault { message } => {
                self.toast(form::friendly_fault(&message), Tone::Warning);
                self.request_status();
            }
            EngineEvent::ConfigChanged => {
                self.send(Command::GetConfig, Tag::Config);
                self.send(Command::ListBackends, Tag::Backends);
                self.request_status();
            }
        }
    }

    fn reply(&mut self, tag: Tag, result: Result<Response, String>) {
        if crate::worker::trace_enabled() {
            match &result {
                Ok(r) => eprintln!("[ui-trace] reply {tag:?} ok={} {:?}", r.ok, r.error),
                Err(e) => eprintln!("[ui-trace] reply {tag:?} transport error {e}"),
            }
        }
        if tag == Tag::Status {
            self.status_in_flight = false;
            if std::mem::take(&mut self.status_again) {
                self.request_status();
            }
        }
        let resp = match result {
            Ok(r) if r.ok => r,
            Ok(r) => {
                let message = r.error.map(|e| e.message).unwrap_or_else(|| "Request failed".into());
                self.failed(tag, message);
                return;
            }
            Err(e) => {
                self.failed(tag, e);
                return;
            }
        };
        match (tag, resp.data) {
            (Tag::Status, Some(ResponseData::Status(s))) => {
                self.status = Some(*s);
                self.update_poll_timer();
                self.render();
            }
            (Tag::Config, Some(ResponseData::Config(cfg))) | (Tag::ConfigSaved, Some(ResponseData::Config(cfg))) => {
                if tag == Tag::ConfigSaved {
                    self.save_in_flight = false;
                }
                self.config = Some(*cfg);
                if !self.edit_pending && !self.save_in_flight {
                    self.load_form();
                }
                self.render();
            }
            (Tag::Backends, Some(ResponseData::Backends(list))) => {
                self.backends = list;
                self.render_backends(true);
            }
            (Tag::BackendSwitched, Some(ResponseData::BackendSwitched(info))) => {
                self.backend_busy = false;
                let label = info
                    .active_backend
                    .and_then(|k| BACKENDS.iter().position(|b| *b == k))
                    .map_or("the selected method", |i| BACKEND_LABELS[i]);
                self.toast(format!("Now using {label}."), Tone::Success);
                for w in info.warnings {
                    self.toast(w, Tone::Warning);
                }
                self.send(Command::ListBackends, Tag::Backends);
                self.request_status();
            }
            (Tag::ActiveWindowForTarget, Some(ResponseData::Window(w))) => {
                self.finish_pick(w);
            }
            (Tag::TargetSet, _) => {
                self.request_status();
                self.send(Command::GetConfig, Tag::Config);
            }
            (Tag::Start, _) | (Tag::Stop, _) => self.request_status(),
            (Tag::Quit, _) => {
                let _ = slint::quit_event_loop();
            }
            _ => {}
        }
    }

    fn failed(&mut self, tag: Tag, message: String) {
        match tag {
            Tag::Status => {}
            Tag::ConfigSaved => {
                self.save_in_flight = false;
                self.toast(format!("Not saved: {message}"), Tone::Error);
                // Show what the engine actually uses.
                self.send(Command::GetConfig, Tag::Config);
            }
            Tag::BackendSwitched => {
                self.backend_busy = false;
                self.render_backends(false);
                if let Some(ui) = self.ui() {
                    let f = ui.global::<Form>();
                    f.set_backend_status(message.into());
                    f.set_backend_tone(Tone::Error);
                }
                self.send(Command::ListBackends, Tag::Backends);
            }
            Tag::Quit => {
                let _ = slint::quit_event_loop();
            }
            Tag::ActiveWindowForTarget | Tag::TargetSet => {
                self.countdown = 0;
                self.render_window_card();
                self.toast(message, Tone::Error);
            }
            _ => self.toast(message, Tone::Error),
        }
    }

    // ------------------------------------------------------------------
    // Rendering
    // ------------------------------------------------------------------

    fn render(&mut self) {
        let Some(ui) = self.ui() else { return };
        let g = ui.global::<Ui>();
        let running = self.running();
        g.set_connected(self.online());
        g.set_running(running);
        ui.global::<Form>().set_editable(self.online() && !running);

        let (problem, detail, action, busy) = match &self.link {
            Link::Online => (String::new(), String::new(), String::new(), false),
            Link::Connecting => ("Connecting to Pulse…".into(), String::new(), String::new(), true),
            Link::Starting { .. } => (
                "Starting Pulse…".into(),
                "Pulse runs in the background, so your hotkeys keep working after you close this window.".into(),
                String::new(),
                true,
            ),
            Link::StartFailed(e) => ("Pulse couldn't start".into(), e.clone(), "Try again".into(), false),
            Link::Stopped => (
                "Pulse stopped running".into(),
                "Hotkeys don't work until Pulse runs again.".into(),
                "Start Pulse".into(),
                false,
            ),
            Link::Older(_) => (
                "An older version of Pulse is running".into(),
                "Restart it to use this window. Clicking stops while it restarts.".into(),
                "Restart Pulse".into(),
                false,
            ),
            Link::Restarting { .. } => ("Restarting Pulse…".into(), String::new(), "Restart Pulse".into(), true),
        };
        g.set_problem(problem.into());
        g.set_problem_detail(detail.into());
        g.set_problem_action(action.into());
        g.set_problem_busy(busy);

        let (pill, tone) = match (&self.link, self.status.as_ref()) {
            (Link::Online, Some(s)) => match s.state {
                EngineState::Running => ("Clicking", Tone::Accent),
                EngineState::Stopping => ("Stopping", Tone::Warning),
                EngineState::Error => ("Problem", Tone::Error),
                EngineState::Ready if s.can_start => ("Ready", Tone::Success),
                EngineState::Booting => ("Starting…", Tone::Neutral),
                EngineState::ShuttingDown => ("Closing…", Tone::Neutral),
                _ => ("Not ready", Tone::Warning),
            },
            (Link::Online, None)
            | (Link::Connecting, _)
            | (Link::Starting { .. }, _)
            | (Link::Restarting { .. }, _) => ("Connecting…", Tone::Neutral),
            (Link::Older(_), _) | (Link::StartFailed(_), _) => ("Problem", Tone::Error),
            (Link::Stopped, _) => ("Not running", Tone::Warning),
        };
        g.set_pill_text(pill.into());
        g.set_pill_tone(tone);

        self.render_main_card();
        self.render_window_card();
    }

    fn render_main_card(&self) {
        let Some(ui) = self.ui() else { return };
        let g = ui.global::<Ui>();
        let Some(s) = self.status.as_ref().filter(|_| self.online()) else {
            g.set_button_enabled(false);
            g.set_hint(SharedString::new());
            g.set_detail(SharedString::new());
            return;
        };
        let c = &s.clicker;
        let unit_is_key = c.input == InputKind::Key;
        let (hotkey, stop) = (c.activation_hotkey.name(), c.emergency_stop_hotkey.name());
        if self.running() {
            g.set_button_text("■  Stop".into());
            g.set_button_danger(true);
            g.set_button_enabled(true);
            g.set_hint(
                format!(
                    "{} so far · {hotkey} or {stop} stops",
                    form::count_text(s.counters.current_run_clicks, unit_is_key)
                )
                .into(),
            );
            g.set_hint_tone(Tone::Accent);
            g.set_detail(SharedString::new());
            return;
        }
        g.set_button_text("▶  Start".into());
        g.set_button_danger(false);
        let hold = c.activation_mode == ActivationMode::Hold;
        let (hint, tone, can_click_button) = if let Some(blocker) = s.start_blocker.clone() {
            (blocker, Tone::Warning, false)
        } else if s.require_focus {
            let game = form::window_name(&s.target);
            let game = if game.is_empty() { "the game".to_owned() } else { game };
            let how = if hold {
                format!("hold {hotkey} to click")
            } else {
                format!("press {hotkey} to start")
            };
            (
                format!("Switch to {game} and {how} · {stop} stops everything"),
                Tone::Neutral,
                false,
            )
        } else if hold {
            (
                format!("Hold {hotkey} to click · {stop} stops everything"),
                Tone::Neutral,
                true,
            )
        } else {
            (
                format!("Press {hotkey} to start or stop · {stop} stops everything"),
                Tone::Neutral,
                true,
            )
        };
        g.set_button_enabled(can_click_button && s.can_start);
        g.set_hint(hint.into());
        g.set_hint_tone(tone);
        g.set_detail(
            s.last_stop
                .as_ref()
                .map(|l| form::last_run_text(l, unit_is_key))
                .unwrap_or_default()
                .into(),
        );
    }

    fn render_window_card(&self) {
        let Some(ui) = self.ui() else { return };
        let f = ui.global::<Form>();
        f.set_picking(self.countdown > 0);
        f.set_pick_text(if self.countdown > 0 {
            format!("Picking in {}…", self.countdown).into()
        } else {
            "Pick window".into()
        });
        let Some(s) = self.status.as_ref().filter(|_| self.online()) else {
            f.set_window_name(SharedString::new());
            f.set_window_state(SharedString::new());
            f.set_window_note(SharedString::new());
            return;
        };
        let name = form::window_name(&s.target);
        f.set_window_name(name.clone().into());
        if name.is_empty() {
            f.set_window_state(SharedString::new());
        } else if s.target_focused {
            f.set_window_state("In front now".into());
            f.set_window_tone(Tone::Success);
        } else {
            f.set_window_state("Not in front".into());
            f.set_window_tone(Tone::Neutral);
        }
        let (note, tone) = if self.countdown > 0 {
            ("Now click your game window…".to_owned(), Tone::Accent)
        } else if !f.get_require_focus() {
            ("Clicks go to whatever window is in front.".to_owned(), Tone::Warning)
        } else if name.is_empty() {
            ("Pick your game so Pulse only clicks there.".to_owned(), Tone::Neutral)
        } else {
            (String::new(), Tone::Neutral)
        };
        f.set_window_note(note.into());
        f.set_window_note_tone(tone);
    }

    fn render_backends(&self, reset_selection: bool) {
        let Some(ui) = self.ui() else { return };
        let f = ui.global::<Form>();
        f.set_backend_busy(self.backend_busy);
        let active = self.status.as_ref().and_then(|s| s.active_backend);
        if reset_selection {
            let want = active
                .or_else(|| self.status.as_ref().map(|s| s.desired_backend))
                .unwrap_or(BackendKind::XTest);
            let i = BACKENDS.iter().position(|k| *k == want).unwrap_or(0);
            f.set_backend_index(i as i32);
        }
        let i = usize::try_from(f.get_backend_index()).unwrap_or(0).min(1);
        let kind = BACKENDS[i];
        let info = self.backends.iter().find(|b| b.kind == kind);
        let in_use = active == Some(kind);
        let (status, tone) = match info {
            Some(_) if in_use => ("In use · working".to_owned(), Tone::Success),
            Some(b) => (b.availability.label().to_owned(), tone_for(b.availability)),
            None => (String::new(), Tone::Neutral),
        };
        f.set_backend_status(status.into());
        f.set_backend_tone(tone);
        let help = info
            .map(|b| match &b.remediation {
                Some(r) if !in_use => format!("{} How to fix: {r}", b.description),
                _ => b.description.clone(),
            })
            .unwrap_or_default();
        f.set_backend_help(help.into());
        f.set_backend_can_switch(!in_use && info.is_some());
    }

    /// Show the engine's configuration in the form.
    fn load_form(&mut self) {
        let Some(cfg) = self.config.clone() else { return };
        let Some(ui) = self.ui() else { return };
        let v = form::form_from_config(&cfg);
        let f = ui.global::<Form>();
        f.set_click_index(v.click_index);
        f.set_key_name(v.key.clone().into());
        f.set_key_index(key_index(&v.key));
        f.set_interval_ms(v.interval_ms);
        f.set_hold_ms(v.hold_ms);
        f.set_mode_index(v.mode_index);
        f.set_hotkey(v.hotkey.clone().into());
        f.set_hotkey_index(key_index(&v.hotkey));
        f.set_emergency(cfg.clicker.emergency_stop_hotkey.name().into());
        f.set_count(v.count);
        f.set_duration_s(v.duration_s);
        f.set_advanced(v.advanced);
        f.set_require_focus(v.require_focus);
        self.validate_form(&v);
    }

    fn read_form(&self) -> Option<FormValues> {
        let ui = self.ui()?;
        let f = ui.global::<Form>();
        Some(FormValues {
            click_index: f.get_click_index(),
            key: f.get_key_name().to_string(),
            interval_ms: f.get_interval_ms(),
            hold_ms: f.get_hold_ms(),
            mode_index: f.get_mode_index(),
            hotkey: f.get_hotkey().to_string(),
            count: f.get_count(),
            duration_s: f.get_duration_s(),
            advanced: f.get_advanced(),
            require_focus: f.get_require_focus(),
        })
    }

    /// Validate the form locally. Returns the candidate config if valid.
    fn validate_form(&self, v: &FormValues) -> Option<Config> {
        let ui = self.ui()?;
        let f = ui.global::<Form>();
        f.set_cps_text(form::cps_text(v.interval_ms).into());
        let base = self.config.clone()?;
        let (candidate, messages) = match form::apply_form(&base, v) {
            Ok(cfg) => {
                let messages = match pulse_config::plan(&cfg) {
                    Ok(p) => Messages::from_issues(&p.warnings),
                    Err(e) => Messages::from_issues(&e.issues),
                };
                (Some(cfg), messages)
            }
            Err(e) => (
                None,
                Messages {
                    errors: vec![(e.field, e.message)],
                    warnings: Vec::new(),
                },
            ),
        };
        f.set_err_click(messages.error(Field::Click).into());
        f.set_err_key(messages.error(Field::Key).into());
        f.set_warn_key(messages.warning(Field::Key).into());
        f.set_err_speed(messages.error(Field::Speed).into());
        f.set_warn_speed(messages.warning(Field::Speed).into());
        f.set_err_hold(messages.error(Field::Hold).into());
        f.set_err_hotkey(messages.error(Field::Hotkey).into());
        f.set_err_count(messages.error(Field::Count).into());
        f.set_err_duration(messages.error(Field::Duration).into());
        let general = [Field::General, Field::Window]
            .iter()
            .map(|x| messages.error(*x))
            .find(|m| !m.is_empty())
            .unwrap_or_default();
        f.set_err_general(general.into());
        candidate.filter(|_| !messages.has_errors())
    }

    // ------------------------------------------------------------------
    // User actions
    // ------------------------------------------------------------------

    pub fn main_button(&mut self) {
        if !self.online() {
            return;
        }
        if self.running() {
            self.send(Command::Stop, Tag::Stop);
        } else {
            self.send(Command::Start, Tag::Start);
        }
    }

    pub fn problem_action(&mut self) {
        match self.link {
            Link::Older(_) => self.restart_older_engine(),
            Link::StartFailed(_) | Link::Stopped => self.start_engine(),
            _ => {}
        }
    }

    pub fn form_changed(&mut self) {
        let Some(v) = self.read_form() else { return };
        self.render_window_card();
        let valid = self.validate_form(&v);
        if valid.is_some() {
            self.edit_pending = true;
            self.apply_timer
                .start(TimerMode::SingleShot, APPLY_DELAY, || with(|c| c.apply_now()));
        } else {
            // Nothing will be sent until the form is valid again.
            self.edit_pending = true;
            self.apply_timer.stop();
        }
    }

    fn apply_now(&mut self) {
        self.edit_pending = false;
        if self.running() || !self.online() {
            return;
        }
        let Some(v) = self.read_form() else { return };
        let Some(candidate) = self.validate_form(&v) else {
            return;
        };
        if self.config.as_ref() == Some(&candidate) {
            return;
        }
        self.save_in_flight = true;
        self.send(
            Command::SetConfig {
                config: Box::new(candidate),
            },
            Tag::ConfigSaved,
        );
    }

    pub fn key_captured(&mut self, text: &str) {
        match crate::keys::key_name_from_slint(text) {
            Some(name) => self.key_chosen(&name),
            None => self.toast("That key can't be used here. Pick one from the list.", Tone::Warning),
        }
    }

    pub fn key_chosen(&mut self, name: &str) {
        if let Some(ui) = self.ui() {
            let f = ui.global::<Form>();
            f.set_key_name(name.into());
            f.set_key_index(key_index(name));
        }
        self.form_changed();
    }

    pub fn hotkey_captured(&mut self, text: &str) {
        match crate::keys::key_name_from_slint(text) {
            Some(name) => self.hotkey_chosen(&name),
            None => self.toast("That key can't be used as a hotkey.", Tone::Warning),
        }
    }

    pub fn hotkey_chosen(&mut self, name: &str) {
        if let Some(ui) = self.ui() {
            let f = ui.global::<Form>();
            f.set_hotkey(name.into());
            f.set_hotkey_index(key_index(name));
        }
        self.form_changed();
    }

    pub fn pick_window(&mut self) {
        if !self.online() || self.countdown > 0 {
            return;
        }
        self.countdown = 3;
        self.render_window_card();
        self.countdown_timer
            .start(TimerMode::Repeated, Duration::from_secs(1), || {
                with(|c| c.countdown_tick())
            });
    }

    fn countdown_tick(&mut self) {
        self.countdown = self.countdown.saturating_sub(1);
        if self.countdown == 0 {
            self.countdown_timer.stop();
            self.send(Command::ActiveWindow, Tag::ActiveWindowForTarget);
        }
        self.render_window_card();
    }

    fn finish_pick(&mut self, w: Option<pulse_core::WindowInfo>) {
        match w {
            Some(w) if form::is_own_window(&w) => {
                self.toast("Pulse's own window was in front. Try again.", Tone::Warning)
            }
            Some(w) => {
                let rule = TargetRule::from_window(&w);
                let name = form::window_name(&rule);
                self.send(Command::SetTarget { rule }, Tag::TargetSet);
                self.toast(format!("Game window set: {name}"), Tone::Success);
            }
            None => self.toast("No window was in front. Try again.", Tone::Warning),
        }
        self.render_window_card();
    }

    pub fn more_toggled(&mut self) {
        let open = self.ui().is_some_and(|ui| ui.global::<Ui>().get_more_open());
        if open && self.online() {
            self.send(Command::ListBackends, Tag::Backends);
        }
    }

    pub fn backend_chosen(&mut self, _index: i32) {
        self.render_backends(false);
    }

    pub fn backend_switch(&mut self) {
        let Some(ui) = self.ui() else { return };
        if self.running() || self.backend_busy {
            return;
        }
        let i = usize::try_from(ui.global::<Form>().get_backend_index())
            .unwrap_or(0)
            .min(1);
        self.backend_busy = true;
        self.render_backends(false);
        self.send(Command::SelectBackend { backend: BACKENDS[i] }, Tag::BackendSwitched);
    }

    pub fn quit_pulse(&mut self) {
        if !self.online() {
            let _ = slint::quit_event_loop();
            return;
        }
        self.send(Command::Shutdown, Tag::Quit);
        // Leave even if the engine does not answer.
        self.quit_timer
            .start(TimerMode::SingleShot, Duration::from_millis(1500), || {
                let _ = slint::quit_event_loop();
            });
    }
}

/// Connect the Slint callbacks to the controller.
pub fn wire(ui: &MainWindow) {
    let a = ui.global::<Actions>();
    a.on_main_button(|| with(|c| c.main_button()));
    a.on_problem_action(|| with(|c| c.problem_action()));
    a.on_form_changed(|| with(|c| c.form_changed()));
    a.on_key_captured(|t| with(move |c| c.key_captured(&t)));
    a.on_key_chosen(|t| with(move |c| c.key_chosen(&t)));
    a.on_hotkey_captured(|t| with(move |c| c.hotkey_captured(&t)));
    a.on_hotkey_chosen(|t| with(move |c| c.hotkey_chosen(&t)));
    a.on_pick_window(|| with(|c| c.pick_window()));
    a.on_more_toggled(|| with(|c| c.more_toggled()));
    a.on_backend_chosen(|i| with(move |c| c.backend_chosen(i)));
    a.on_backend_switch(|| with(|c| c.backend_switch()));
    a.on_quit_pulse(|| with(|c| c.quit_pulse()));
}
