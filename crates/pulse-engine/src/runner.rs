//! The timing/output thread.
//!
//! The runner exclusively owns the active input backend and the executor,
//! so every termination path can release input consistently. It sleeps in
//! `epoll_wait` on two descriptors:
//!
//! - a `timerfd` armed with absolute `CLOCK_MONOTONIC` deadlines
//!   (`TFD_TIMER_ABSTIME`), re-armed after every wakeup;
//! - an `eventfd` signalled by the control thread when a command is queued.
//!
//! Hot path per event: one clock read, two atomic loads (cancel generation,
//! focus), the executor step, one backend call, relaxed metric increments and
//! one `timerfd_settime`. No allocation, locking, logging, IPC or formatting.

use std::os::fd::{AsFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread::JoinHandle;

use pulse_core::scheduler::{Executor, FinishReason, Next, RunStats};
use pulse_core::{HeldInputs, LogicalInput, Nanos, Program, StopReason};
use pulse_input::{BackendHealth, InputBackend, InputError};
use rustix::event::{EventfdFlags, epoll, eventfd};
use rustix::io::Errno;
use rustix::time::{Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, timerfd_create, timerfd_settime};

use crate::clock::{monotonic_now, suspend_offset, to_timespec};
use crate::metrics::Metrics;

/// Suspend is assumed when BOOTTIME advanced this much more than MONOTONIC.
const SUSPEND_THRESHOLD: Nanos = 500_000_000;
/// Executor steps per wakeup before yielding to the event loop.
const MAX_STEPS_PER_WAKE: usize = 64;

const TOKEN_TIMER: u64 = 1;
const TOKEN_WAKE: u64 = 2;

pub fn reason_to_u8(r: StopReason) -> u8 {
    StopReason::ALL.iter().position(|x| *x == r).unwrap_or(0) as u8
}

pub fn reason_from_u8(v: u8) -> StopReason {
    StopReason::ALL.get(v as usize).copied().unwrap_or(StopReason::UserStop)
}

/// State shared between control and runner threads.
#[derive(Default)]
pub struct Shared {
    /// Highest cancelled run id; a run is cancelled when `id <= cancel`.
    pub cancel: AtomicU64,
    pub cancel_reason: AtomicU8,
    /// Game window currently focused (maintained by the control thread).
    pub focus_ok: AtomicBool,
    pub metrics: Metrics,
}

pub struct StartSpec {
    pub run_id: u64,
    pub program: Program,
    pub hold_mode: bool,
    pub require_focus: bool,
    pub stop_on_suspend: bool,
    /// When the triggering hotkey event was received (trigger latency).
    pub trigger_at: Option<Nanos>,
}

pub enum RunnerCmd {
    Start(StartSpec),
    Stop {
        run_id: u64,
        reason: StopReason,
        requested_at: Nanos,
    },
    /// Hold mode: the hotkey was released.
    TriggerReleased {
        run_id: u64,
    },
    Install(Box<dyn InputBackend>),
    /// Release everything and hand the backend back (None while running).
    Take(SyncSender<Option<Box<dyn InputBackend>>>),
    Check(SyncSender<Option<Result<BackendHealth, InputError>>>),
    Supports(HeldInputs, SyncSender<Option<Result<(), InputError>>>),
    Shutdown,
}

#[derive(Debug)]
pub struct Finished {
    pub run_id: u64,
    pub reason: StopReason,
    pub stats: RunStats,
    pub release_ok: bool,
    pub duration: Nanos,
}

#[derive(Debug)]
pub enum RunnerEvent {
    Started { run_id: u64 },
    StartRejected { run_id: u64, message: String },
    Finished(Finished),
    BackendFault { message: String },
}

fn finish_to_stop(r: FinishReason) -> StopReason {
    match r {
        FinishReason::Completed => StopReason::Completed,
        FinishReason::DurationElapsed => StopReason::DurationElapsed,
        FinishReason::Cancelled => StopReason::UserStop,
    }
}

struct Run {
    id: u64,
    exec: Executor,
    started_at: Nanos,
    hold_mode: bool,
    require_focus: bool,
    stop_on_suspend: bool,
    suspend_base: Nanos,
    trigger_at: Option<Nanos>,
    first_emit: bool,
    armed: Option<Nanos>,
    stop_requested_at: Option<Nanos>,
}

struct Runner {
    rx: Receiver<RunnerCmd>,
    events: Sender<RunnerEvent>,
    events_fd: Arc<OwnedFd>,
    wake_fd: Arc<OwnedFd>,
    timer: OwnedFd,
    epoll: OwnedFd,
    shared: Arc<Shared>,
    backend: Option<Box<dyn InputBackend>>,
    run: Option<Run>,
    shutdown: bool,
}

fn signal(fd: &OwnedFd) {
    // EAGAIN only if the counter would overflow; the reader is awake then.
    let _ = rustix::io::write(fd, &1u64.to_ne_bytes());
}

fn drain_eventfd(fd: &OwnedFd) {
    let mut buf = [0u8; 8];
    let _ = rustix::io::read(fd, &mut buf);
}

impl Runner {
    fn notify(&self, ev: RunnerEvent) {
        if self.events.send(ev).is_ok() {
            signal(&self.events_fd);
        }
    }

    fn arm(&self, deadline: Nanos) {
        let spec = Itimerspec {
            it_interval: to_timespec(0),
            it_value: to_timespec(deadline.max(1)),
        };
        // Cannot fail for a valid fd and timespec.
        let _ = timerfd_settime(&self.timer, TimerfdTimerFlags::ABSTIME, &spec);
    }

    fn disarm(&self) {
        let spec = Itimerspec {
            it_interval: to_timespec(0),
            it_value: to_timespec(0),
        };
        let _ = timerfd_settime(&self.timer, TimerfdTimerFlags::empty(), &spec);
    }

    fn main_loop(&mut self) {
        let mut evs: Vec<epoll::Event> = Vec::with_capacity(4);
        while !self.shutdown {
            evs.clear();
            match epoll::wait(&self.epoll, rustix::buffer::spare_capacity(&mut evs), None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(_) => break,
            }
            let mut timer_fired = false;
            for e in &evs {
                match e.data.u64() {
                    TOKEN_WAKE => {
                        drain_eventfd(&self.wake_fd);
                        self.process_commands();
                    }
                    TOKEN_TIMER => timer_fired = true,
                    _ => {}
                }
            }
            if timer_fired {
                if self.run.is_some() {
                    self.step();
                } else {
                    self.disarm();
                }
            }
        }
        self.cleanup();
    }

    fn process_commands(&mut self) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                RunnerCmd::Start(spec) => self.start(spec),
                RunnerCmd::Stop {
                    run_id,
                    reason,
                    requested_at,
                } => {
                    if let Some(run) = self.run.as_mut().filter(|r| r.id == run_id) {
                        run.stop_requested_at.get_or_insert(requested_at);
                        self.cancel_run(reason);
                    }
                }
                RunnerCmd::TriggerReleased { run_id } => {
                    if let Some(run) = self.run.as_mut().filter(|r| r.id == run_id && r.hold_mode) {
                        run.stop_requested_at = Some(monotonic_now());
                        self.cancel_run(StopReason::TriggerReleased);
                    }
                }
                RunnerCmd::Install(b) => {
                    if self.run.is_none() {
                        self.backend = Some(b);
                    }
                }
                RunnerCmd::Take(reply) => {
                    let out = if self.run.is_some() {
                        None
                    } else {
                        self.backend.take().map(|mut b| {
                            let _ = b.release_all(&HeldInputs::new());
                            b
                        })
                    };
                    let _ = reply.send(out);
                }
                RunnerCmd::Check(reply) => {
                    let out = if self.run.is_some() {
                        None
                    } else {
                        self.backend.as_mut().map(|b| b.check().map(|()| b.health()))
                    };
                    let _ = reply.send(out);
                }
                RunnerCmd::Supports(set, reply) => {
                    let _ = reply.send(self.backend.as_ref().map(|b| b.supports(&set)));
                }
                RunnerCmd::Shutdown => {
                    self.shutdown = true;
                }
            }
        }
    }

    fn start(&mut self, spec: StartSpec) {
        if self.run.is_some() {
            self.notify(RunnerEvent::StartRejected {
                run_id: spec.run_id,
                message: "already clicking".into(),
            });
            return;
        }
        if self.backend.is_none() {
            self.notify(RunnerEvent::StartRejected {
                run_id: spec.run_id,
                message: "no input method is initialized".into(),
            });
            return;
        }
        if self.shared.cancel.load(Ordering::Acquire) >= spec.run_id {
            // Cancelled before it started (e.g. emergency stop raced start).
            self.notify(RunnerEvent::Finished(Finished {
                run_id: spec.run_id,
                reason: reason_from_u8(self.shared.cancel_reason.load(Ordering::Acquire)),
                stats: RunStats::default(),
                release_ok: true,
                duration: 0,
            }));
            return;
        }
        let now = monotonic_now();
        self.shared.metrics.current_run_clicks.store(0, Ordering::Relaxed);
        self.shared.metrics.runs.fetch_add(1, Ordering::Relaxed);
        self.run = Some(Run {
            id: spec.run_id,
            exec: Executor::new(spec.program, now),
            started_at: now,
            hold_mode: spec.hold_mode,
            require_focus: spec.require_focus,
            stop_on_suspend: spec.stop_on_suspend,
            suspend_base: if spec.stop_on_suspend { suspend_offset() } else { 0 },
            trigger_at: spec.trigger_at,
            first_emit: false,
            armed: None,
            stop_requested_at: None,
        });
        self.notify(RunnerEvent::Started { run_id: spec.run_id });
        self.step();
    }

    /// Execute everything due. Bounded; re-arms the timer before returning.
    fn step(&mut self) {
        for _ in 0..MAX_STEPS_PER_WAKE {
            let now = monotonic_now();
            let Some(run) = self.run.as_mut() else {
                self.disarm();
                return;
            };
            if let Some(d) = run.armed.take() {
                self.shared.metrics.wake_jitter.record_ns(now.saturating_sub(d));
            }
            if self.shared.cancel.load(Ordering::Acquire) >= run.id {
                let reason = reason_from_u8(self.shared.cancel_reason.load(Ordering::Acquire));
                run.stop_requested_at.get_or_insert(now);
                self.cancel_run(reason);
                return;
            }
            if run.require_focus && !self.shared.focus_ok.load(Ordering::Acquire) {
                self.cancel_run(StopReason::FocusLost);
                return;
            }
            if run.stop_on_suspend && suspend_offset().saturating_sub(run.suspend_base) > SUSPEND_THRESHOLD {
                self.cancel_run(StopReason::Suspend);
                return;
            }
            let step = run.exec.poll(now);
            if let Some(ev) = step.emit {
                if !self.submit(ev) {
                    return;
                }
            }
            match step.next {
                Next::At(t) if t <= now => continue,
                Next::At(t) => {
                    if let Some(run) = self.run.as_mut() {
                        run.armed = Some(t);
                    }
                    self.arm(t);
                    return;
                }
                Next::Done(r) => {
                    self.finish(finish_to_stop(r), true);
                    return;
                }
            }
        }
        // Yield to the event loop; the timer fires immediately.
        self.arm(monotonic_now());
    }

    /// Submit one event. Returns false if the run was ended by a fault.
    fn submit(&mut self, ev: LogicalInput) -> bool {
        let Some(backend) = self.backend.as_mut() else {
            self.fault(InputError::Disconnected("backend missing".into()), ev);
            return false;
        };
        let t0 = monotonic_now();
        let result = backend.emit(std::slice::from_ref(&ev));
        let t1 = monotonic_now();
        let m = &self.shared.metrics;
        m.emit_duration.record_ns(t1 - t0);
        match result {
            Ok(()) => {
                m.submitted_events.fetch_add(1, Ordering::Relaxed);
                if ev.is_press() {
                    m.submitted_clicks.fetch_add(1, Ordering::Relaxed);
                    m.current_run_clicks.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(run) = self.run.as_mut() {
                    if !run.first_emit {
                        run.first_emit = true;
                        if let Some(tt) = run.trigger_at {
                            m.trigger_latency.record_ns(t1.saturating_sub(tt));
                        }
                    }
                }
                true
            }
            Err(e) => {
                self.fault(e, ev);
                false
            }
        }
    }

    /// Backend failure mid-run: release the target conservatively (it may
    /// or may not have been pressed), then end the run.
    fn fault(&mut self, e: InputError, failed: LogicalInput) {
        let mut conservative = HeldInputs::new();
        conservative.apply_lenient(&failed.pressable().0.press());
        let release_ok = self
            .backend
            .as_mut()
            .map(|b| b.release_all(&conservative).is_ok())
            .unwrap_or(false);
        self.shared.metrics.backend_errors.fetch_add(1, Ordering::Relaxed);
        self.notify(RunnerEvent::BackendFault { message: e.to_string() });
        self.finish(StopReason::BackendFault, release_ok);
    }

    /// Cancel the active run: emit the release if the target is held,
    /// cross-check the backend ledger, then report.
    fn cancel_run(&mut self, reason: StopReason) {
        let Some(run) = self.run.as_mut() else { return };
        let target = run.exec.program().target;
        let step = run.exec.cancel();
        let mut release_ok = true;
        if let Some(ev) = step.emit {
            let ok = self
                .backend
                .as_mut()
                .is_some_and(|b| b.emit(std::slice::from_ref(&ev)).is_ok());
            if !ok {
                let mut all = HeldInputs::new();
                all.apply_lenient(&target.press());
                release_ok = self.backend.as_mut().is_some_and(|b| b.release_all(&all).is_ok());
            }
        }
        self.finish(reason, release_ok);
    }

    fn finish(&mut self, reason: StopReason, mut release_ok: bool) {
        self.disarm();
        let Some(run) = self.run.take() else { return };
        // Cross-check: anything the backend still believes is held.
        if let Some(b) = self.backend.as_mut() {
            if !b.held().is_empty() || run.exec.is_held() {
                let mut extra = HeldInputs::new();
                if run.exec.is_held() {
                    extra.apply_lenient(&run.exec.program().target.press());
                }
                release_ok &= b.release_all(&extra).is_ok();
            }
        }
        let now = monotonic_now();
        if let Some(t) = run.stop_requested_at {
            self.shared.metrics.stop_latency.record_ns(now.saturating_sub(t));
        }
        let stats = *run.exec.stats();
        self.shared
            .metrics
            .missed_deadlines
            .fetch_add(stats.missed_deadlines, Ordering::Relaxed);
        self.notify(RunnerEvent::Finished(Finished {
            run_id: run.id,
            reason,
            stats,
            release_ok,
            duration: now.saturating_sub(run.started_at),
        }));
    }

    fn cleanup(&mut self) {
        if self.run.is_some() {
            self.cancel_run(StopReason::Shutdown);
        }
        if let Some(mut b) = self.backend.take() {
            let _ = b.release_all(&HeldInputs::new());
        }
    }
}

/// Control-thread handle to the runner.
pub struct RunnerHandle {
    tx: Sender<RunnerCmd>,
    wake_fd: Arc<OwnedFd>,
    events_rx: Receiver<RunnerEvent>,
    events_fd: Arc<OwnedFd>,
    pub shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl RunnerHandle {
    pub fn spawn(shared: Arc<Shared>) -> std::io::Result<RunnerHandle> {
        let (tx, rx) = mpsc::channel();
        let (etx, events_rx) = mpsc::channel();
        let wake_fd = Arc::new(eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC)?);
        let events_fd = Arc::new(eventfd(0, EventfdFlags::NONBLOCK | EventfdFlags::CLOEXEC)?);
        let timer = timerfd_create(
            TimerfdClockId::Monotonic,
            TimerfdFlags::NONBLOCK | TimerfdFlags::CLOEXEC,
        )?;
        let ep = epoll::create(epoll::CreateFlags::CLOEXEC)?;
        epoll::add(
            &ep,
            &timer,
            epoll::EventData::new_u64(TOKEN_TIMER),
            epoll::EventFlags::IN,
        )?;
        epoll::add(
            &ep,
            &*wake_fd,
            epoll::EventData::new_u64(TOKEN_WAKE),
            epoll::EventFlags::IN,
        )?;
        let mut runner = Runner {
            rx,
            events: etx,
            events_fd: Arc::clone(&events_fd),
            wake_fd: Arc::clone(&wake_fd),
            timer,
            epoll: ep,
            shared: Arc::clone(&shared),
            backend: None,
            run: None,
            shutdown: false,
        };
        let thread = std::thread::Builder::new()
            .name("pulse-runner".into())
            .spawn(move || runner.main_loop())?;
        Ok(RunnerHandle {
            tx,
            wake_fd,
            events_rx,
            events_fd,
            shared,
            thread: Some(thread),
        })
    }

    pub fn events_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.events_fd.as_fd()
    }

    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    pub fn send(&self, cmd: RunnerCmd) -> bool {
        if self.tx.send(cmd).is_err() {
            return false;
        }
        signal(&self.wake_fd);
        true
    }

    /// Cancel a run: publish the cancellation generation first (checked by
    /// the runner before every event), then wake it with a stop command.
    pub fn cancel(&self, run_id: u64, reason: StopReason) {
        self.shared.cancel_reason.store(reason_to_u8(reason), Ordering::Release);
        self.shared.cancel.fetch_max(run_id, Ordering::AcqRel);
        self.send(RunnerCmd::Stop {
            run_id,
            reason,
            requested_at: monotonic_now(),
        });
    }

    fn sync<T>(&self, make: impl FnOnce(SyncSender<T>) -> RunnerCmd) -> Option<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        if !self.send(make(tx)) {
            return None;
        }
        rx.recv_timeout(std::time::Duration::from_secs(5)).ok()
    }

    /// Take the backend after releasing everything (None while a run is
    /// active or when no backend is installed).
    pub fn take_backend(&self) -> Option<Box<dyn InputBackend>> {
        self.sync(RunnerCmd::Take).flatten()
    }

    pub fn install_backend(&self, b: Box<dyn InputBackend>) -> bool {
        self.send(RunnerCmd::Install(b))
    }

    pub fn check_backend(&self) -> Option<Result<BackendHealth, InputError>> {
        self.sync(RunnerCmd::Check).flatten()
    }

    pub fn supports(&self, set: HeldInputs) -> Option<Result<(), InputError>> {
        self.sync(|tx| RunnerCmd::Supports(set, tx)).flatten()
    }

    pub fn drain_events(&self) -> Vec<RunnerEvent> {
        drain_eventfd(&self.events_fd);
        self.events_rx.try_iter().collect()
    }

    /// Stop the runner thread: cancels any run, releases input, drops the
    /// backend (closing X11 connections / destroying virtual devices).
    pub fn shutdown(&mut self) {
        self.send(RunnerCmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RunnerHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
