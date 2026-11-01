//! `pulse-bench`: reproducible measurements for Pulse (see `docs/benchmarks.md`).
//!
//! Every subcommand prints a JSON report with raw-sample statistics (exact
//! percentiles, not histogram buckets). Commands that inject input refuse to
//! target the live desktop unless explicitly told to.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::IoSlice;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use pulse_core::clicker::compile;
use pulse_core::scheduler::Executor;
use pulse_core::{BackendKind, ClickerSpec, HeldInputs, KeyState, Limits, LogicalInput, MouseButton, Pressable};
use pulse_input::xtest::encode_fake_input;
use pulse_input::{InputBackend, InputError, UinputBackend, XTestBackend};
use rustix::time::{ClockId, Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec};
use serde_json::json;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::ConnectionExt as _;

// ---------------------------------------------------------------------------
// Counting allocator (only affects this benchmark binary)
// ---------------------------------------------------------------------------

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarding the caller's contract to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarding the caller's contract to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: forwarding the caller's contract to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_ns() -> u64 {
    let t = rustix::time::clock_gettime(ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

fn ts(ns: u64) -> Timespec {
    Timespec {
        tv_sec: (ns / 1_000_000_000) as i64,
        tv_nsec: (ns % 1_000_000_000) as i64,
    }
}

/// Exact statistics over raw samples (ns), reported in µs.
fn stats(mut v: Vec<u64>) -> serde_json::Value {
    if v.is_empty() {
        return json!({"count": 0});
    }
    v.sort_unstable();
    let n = v.len();
    let pct = |p: f64| v[(((n as f64) * p).ceil() as usize).clamp(1, n) - 1] as f64 / 1e3;
    let mean = v.iter().map(|x| *x as f64).sum::<f64>() / n as f64 / 1e3;
    let var = v.iter().map(|x| (*x as f64 / 1e3 - mean).powi(2)).sum::<f64>() / n as f64;
    json!({
        "count": n,
        "min_us": v[0] as f64 / 1e3,
        "p50_us": pct(0.50),
        "p95_us": pct(0.95),
        "p99_us": pct(0.99),
        "p999_us": pct(0.999),
        "max_us": v[n - 1] as f64 / 1e3,
        "mean_us": mean,
        "stddev_us": var.sqrt(),
    })
}

fn environment() -> serde_json::Value {
    let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
    let cpu = read("/proc/cpuinfo")
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_owned());
    let kernel = read("/proc/sys/kernel/osrelease").trim().to_owned();
    let governor = read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
        .trim()
        .to_owned();
    json!({
        "cpu_model": cpu,
        "kernel": kernel,
        "governor": governor,
        "nproc": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        "build": if cfg!(debug_assertions) { "debug" } else { "release" },
        "session": std::env::var("XDG_SESSION_TYPE").ok(),
    })
}

struct Args(Vec<String>);

impl Args {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .position(|a| a == name)
            .and_then(|i| self.0.get(i + 1))
            .map(String::as_str)
    }
    fn num(&self, name: &str, default: u64) -> Result<u64> {
        match self.get(name) {
            Some(v) => v.parse().with_context(|| format!("{name} expects a number")),
            None => Ok(default),
        }
    }
    fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }
}

/// Refuse to inject into the live session unless explicitly allowed.
fn checked_display(args: &Args) -> Result<String> {
    let Some(d) = args.get("--display") else {
        bail!("--display is required (use an isolated server such as Xvfb :99)");
    };
    let live = std::env::var("DISPLAY").unwrap_or_default();
    if d == live && !args.flag("--allow-live-display") {
        bail!(
            "{d} is your live desktop; injected clicks would land in real windows. Use Xvfb or pass --allow-live-display"
        );
    }
    Ok(d.to_owned())
}

// ---------------------------------------------------------------------------
// timer: absolute timerfd vs clock_nanosleep vs relative sleep
// ---------------------------------------------------------------------------

fn bench_timer(args: &Args) -> Result<serde_json::Value> {
    let interval = args.num("--interval-us", 10_000)? * 1_000;
    let count = args.num("--count", 1_000)? as usize;

    // timerfd + TFD_TIMER_ABSTIME (what the runner uses).
    let tfd = rustix::time::timerfd_create(TimerfdClockId::Monotonic, TimerfdFlags::CLOEXEC)?;
    let mut tfd_late = Vec::with_capacity(count);
    let t0 = now_ns() + interval;
    for n in 0..count as u64 {
        let d = t0 + n * interval;
        rustix::time::timerfd_settime(
            &tfd,
            TimerfdTimerFlags::ABSTIME,
            &Itimerspec {
                it_interval: ts(0),
                it_value: ts(d),
            },
        )?;
        let mut buf = [0u8; 8];
        rustix::io::read(&tfd, &mut buf)?;
        tfd_late.push(now_ns().saturating_sub(d));
    }
    let tfd_end_drift = now_ns() as i64 - (t0 + (count as u64 - 1) * interval) as i64;

    // clock_nanosleep(TIMER_ABSTIME).
    let mut cns_late = Vec::with_capacity(count);
    let t0 = now_ns() + interval;
    for n in 0..count as u64 {
        let d = t0 + n * interval;
        let _ = rustix::thread::clock_nanosleep_absolute(rustix::thread::ClockId::Monotonic, &ts(d));
        cns_late.push(now_ns().saturating_sub(d));
    }

    // Naive relative sleep: `sleep(T)` after each action accumulates drift.
    let start = now_ns();
    for _ in 0..count {
        std::thread::sleep(Duration::from_nanos(interval));
    }
    let relative_drift = (now_ns() - start) as i64 - (count as u64 * interval) as i64;

    Ok(json!({
        "benchmark": "timer",
        "interval_us": interval / 1_000,
        "count": count,
        "timerfd_abstime_lateness": stats(tfd_late),
        "timerfd_final_offset_us": tfd_end_drift as f64 / 1e3,
        "clock_nanosleep_abstime_lateness": stats(cns_late),
        "relative_sleep_accumulated_drift_us": relative_drift as f64 / 1e3,
        "environment": environment(),
    }))
}

// ---------------------------------------------------------------------------
// emit: per-batch submission cost for each backend
// ---------------------------------------------------------------------------

fn grab_uinput_nodes(be: &UinputBackend) -> Result<Vec<evdev::raw_stream::RawDevice>> {
    let mut out = Vec::new();
    for n in be.device_nodes() {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut d = loop {
            match evdev::raw_stream::RawDevice::open(n) {
                Ok(d) => break d,
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => bail!("cannot open {} to grab it: {e}", n.display()),
            }
        };
        // Exclusive grab: injected events reach only this benchmark.
        d.grab().with_context(|| format!("EVIOCGRAB {}", n.display()))?;
        out.push(d);
    }
    Ok(out)
}

fn drain(devs: &mut [evdev::raw_stream::RawDevice]) {
    for d in devs {
        let fd = d.as_fd();
        let mut fds = [rustix::event::PollFd::new(&fd, rustix::event::PollFlags::IN)];
        if rustix::event::poll(&mut fds, Some(&ts(0))).unwrap_or(0) > 0 {
            let _ = d.fetch_events().map(|it| it.count());
        }
    }
}

fn bench_emit(args: &Args) -> Result<serde_json::Value> {
    let kind = args.get("--backend").unwrap_or("xtest");
    let count = args.num("--count", 2_000)? as usize;
    let press = LogicalInput::button(MouseButton::Left, KeyState::Pressed);
    let release = LogicalInput::button(MouseButton::Left, KeyState::Released);
    let mut samples = Vec::with_capacity(count * 2);
    let mut grabbed = Vec::new();
    let t_init = now_ns();
    let mut backend: Box<dyn InputBackend> = match kind {
        "xtest" => Box::new(XTestBackend::open(Some(&checked_display(args)?), false)?),
        "uinput" => {
            let be = UinputBackend::open()?;
            grabbed = grab_uinput_nodes(&be)?;
            Box::new(be)
        }
        other => bail!("unknown backend {other}"),
    };
    let init_us = (now_ns() - t_init) as f64 / 1e3;
    let a0 = allocs();
    for i in 0..count {
        for ev in [press, release] {
            let t = now_ns();
            backend.emit(std::slice::from_ref(&ev))?;
            samples.push(now_ns() - t);
        }
        if i % 8 == 7 {
            drain(&mut grabbed);
        }
    }
    let allocs_per_event = (allocs() - a0) as f64 / (count * 2) as f64;
    backend.check()?;
    Ok(json!({
        "benchmark": "emit",
        "backend": kind,
        "events": count * 2,
        "init_us": init_us,
        "emit_call": stats(samples),
        "allocations_per_event": allocs_per_event,
        "note": "time inside InputBackend::emit for one event (incl. flush/SYN_REPORT); not game-received latency",
        "environment": environment(),
    }))
}

// ---------------------------------------------------------------------------
// alloc: allocations on the hot path
// ---------------------------------------------------------------------------

fn bench_alloc(args: &Args) -> Result<serde_json::Value> {
    let program = compile(
        &ClickerSpec {
            target: Pressable::Button(MouseButton::Left),
            interval_ms: 10,
            hold_ms: 3,
            count: 0,
            duration_ms: 0,
        },
        &Limits::STANDARD,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?
    .program;
    let mut ex = Executor::new(program, 0);
    let mut now = 0;
    let a0 = allocs();
    for _ in 0..100_000 {
        if let pulse_core::Next::At(t) = std::hint::black_box(ex.poll(now)).next {
            now = t;
        }
    }
    let executor_allocs = allocs() - a0;
    let mut report = json!({
        "benchmark": "alloc",
        "executor_polls": 100_000,
        "executor_allocations": executor_allocs,
    });
    if let Some(display) = args.get("--display") {
        let display = checked_display(&Args(vec!["--display".into(), display.into()]))?;
        let (conn, _) = x11rb::connect(Some(&display))?;
        let opcode = conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)?
            .context("no XTEST")?
            .major_opcode;
        let n = 10_000u64;
        // x11rb's generated wrapper.
        let a0 = allocs();
        for i in 0..n {
            let t = if i % 2 == 0 { 4 } else { 5 };
            x11rb::protocol::xtest::fake_input(&conn, t, 1, 0, x11rb::NONE, 0, 0, 0)?;
            if i % 256 == 255 {
                conn.get_input_focus()?.reply()?;
            }
        }
        conn.flush()?;
        let wrapper = (allocs() - a0) as f64 / n as f64;
        // Pulse's pre-serialized request.
        let a0 = allocs();
        for i in 0..n {
            let t = if i % 2 == 0 { 4 } else { 5 };
            let buf = encode_fake_input(opcode, t, 1, x11rb::NONE, 0, 0);
            drop(conn.send_request_without_reply(&[IoSlice::new(&buf)], Vec::new())?);
            if i % 256 == 255 {
                conn.get_input_focus()?.reply()?;
            }
        }
        conn.flush()?;
        let pre = (allocs() - a0) as f64 / n as f64;
        report["xtest_x11rb_wrapper_allocs_per_request"] = json!(wrapper);
        report["xtest_preserialized_allocs_per_request"] = json!(pre);
        report["note"] = json!("allocation counts include the periodic sync round trip, amortized");
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// e2e: XTest submission → X11 test window receives ButtonPress
// ---------------------------------------------------------------------------

fn bench_e2e(args: &Args) -> Result<serde_json::Value> {
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{CreateWindowAux, EventMask, WindowClass};
    let display = checked_display(args)?;
    let count = args.num("--count", 1_000)? as usize;
    let (rx, screen) = x11rb::connect(Some(&display))?;
    let s = rx.setup().roots[screen].clone();
    let win = rx.generate_id()?;
    rx.create_window(
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
    )?;
    rx.map_window(win)?;
    rx.warp_pointer(x11rb::NONE, win, 0, 0, 0, 0, 150, 150)?;
    rx.get_input_focus()?.reply()?;
    std::thread::sleep(Duration::from_millis(50));
    while rx.poll_for_event()?.is_some() {}
    let mut be = XTestBackend::open(Some(&display), false)?;
    let mut lat = Vec::with_capacity(count);
    for _ in 0..count {
        let t = now_ns();
        be.emit(&[LogicalInput::button(MouseButton::Left, KeyState::Pressed)])?;
        loop {
            if let Event::ButtonPress(_) = rx.wait_for_event()? {
                break;
            }
        }
        lat.push(now_ns() - t);
        be.emit(&[LogicalInput::button(MouseButton::Left, KeyState::Released)])?;
        loop {
            if let Event::ButtonRelease(_) = rx.wait_for_event()? {
                break;
            }
        }
    }
    Ok(json!({
        "benchmark": "e2e_xtest",
        "display": display,
        "received_press_latency": stats(lat),
        "note": "backend submit → ButtonPress delivered to an X11 client on the same server; excludes any game processing",
        "environment": environment(),
    }))
}

// ---------------------------------------------------------------------------
// process: CPU time and wakeups of a running engine
// ---------------------------------------------------------------------------

fn proc_sample(pid: u32) -> Result<(f64, u64, u64)> {
    let ticks = rustix::param::clock_ticks_per_second() as f64;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).context("process not found")?;
    let rest = &stat[stat.rfind(')').context("bad stat")? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let cpu = (f[11].parse::<f64>()? + f[12].parse::<f64>()?) / ticks;
    let mut vol = 0;
    let mut invol = 0;
    for task in std::fs::read_dir(format!("/proc/{pid}/task"))? {
        let st = std::fs::read_to_string(task?.path().join("status")).unwrap_or_default();
        for l in st.lines() {
            if let Some(v) = l.strip_prefix("voluntary_ctxt_switches:") {
                vol += v.trim().parse::<u64>().unwrap_or(0);
            } else if let Some(v) = l.strip_prefix("nonvoluntary_ctxt_switches:") {
                invol += v.trim().parse::<u64>().unwrap_or(0);
            }
        }
    }
    Ok((cpu, vol, invol))
}

fn bench_process(args: &Args) -> Result<serde_json::Value> {
    let pid: u32 = args.get("--pid").context("--pid is required")?.parse()?;
    let secs = args.num("--seconds", 30)?;
    let (c0, v0, i0) = proc_sample(pid)?;
    let t0 = now_ns();
    std::thread::sleep(Duration::from_secs(secs));
    let (c1, v1, i1) = proc_sample(pid)?;
    let wall = (now_ns() - t0) as f64 / 1e9;
    let rss = std::fs::read_to_string(format!("/proc/{pid}/status"))?
        .lines()
        .find(|l| l.starts_with("VmRSS:"))
        .and_then(|l| l.split_whitespace().nth(1).map(str::to_owned));
    Ok(json!({
        "benchmark": "process",
        "pid": pid,
        "wall_s": wall,
        "cpu_s": c1 - c0,
        "cpu_percent_of_one_core": (c1 - c0) / wall * 100.0,
        "cpu_resolution_s": 1.0 / rustix::param::clock_ticks_per_second() as f64,
        "voluntary_switches_per_s": (v1 - v0) as f64 / wall,
        "involuntary_switches_per_s": (i1 - i0) as f64 / wall,
        "rss_kib": rss,
        "note": "voluntary context switches across all engine threads approximate wakeups; CPU time has clock-tick resolution",
        "environment": environment(),
    }))
}

// ---------------------------------------------------------------------------
// dispatch: dyn InputBackend vs concrete call
// ---------------------------------------------------------------------------

struct NullBackend(HeldInputs);

impl InputBackend for NullBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::XTest
    }
    #[inline(never)]
    fn emit(&mut self, events: &[LogicalInput]) -> Result<(), InputError> {
        for e in events {
            self.0.apply_lenient(std::hint::black_box(e));
        }
        Ok(())
    }
    fn release_all(&mut self, _extra: &HeldInputs) -> Result<(), InputError> {
        Ok(())
    }
    fn held(&self) -> HeldInputs {
        self.0
    }
    fn check(&mut self) -> Result<(), InputError> {
        Ok(())
    }
    fn supports(&self, _set: &HeldInputs) -> Result<(), InputError> {
        Ok(())
    }
    fn health(&self) -> pulse_input::BackendHealth {
        pulse_input::BackendHealth {
            kind: BackendKind::XTest,
            healthy: true,
            detail: String::new(),
            last_error: None,
        }
    }
}

fn bench_dispatch(_args: &Args) -> Result<serde_json::Value> {
    let n = 10_000_000u64;
    let evs = [
        LogicalInput::button(MouseButton::Left, KeyState::Pressed),
        LogicalInput::button(MouseButton::Left, KeyState::Released),
    ];
    let mut concrete = NullBackend(HeldInputs::new());
    let t = now_ns();
    for i in 0..n {
        concrete.emit(std::slice::from_ref(&evs[(i & 1) as usize]))?;
    }
    let direct = (now_ns() - t) as f64 / n as f64;
    let mut boxed: Box<dyn InputBackend> = Box::new(NullBackend(HeldInputs::new()));
    let boxed = std::hint::black_box(&mut boxed);
    let t = now_ns();
    for i in 0..n {
        boxed.emit(std::slice::from_ref(&evs[(i & 1) as usize]))?;
    }
    let dynamic = (now_ns() - t) as f64 / n as f64;
    Ok(json!({
        "benchmark": "dispatch",
        "calls": n,
        "direct_ns_per_call": direct,
        "dyn_ns_per_call": dynamic,
        "note": "compare with emit_call p50 of a real backend (tens of µs): dispatch is negligible",
        "environment": environment(),
    }))
}

const USAGE: &str = "\
Usage: pulse-bench <COMMAND> [OPTIONS]

Commands:
  timer     [--interval-us N] [--count N]           timer wakeup lateness (timerfd vs clock_nanosleep) and relative-sleep drift
  emit      --backend xtest|uinput [--display D] [--count N]
                                                    per-event backend submit cost (uinput devices are grabbed: events never reach the desktop)
  alloc     [--display D]                           allocations per executor poll and per XTest request
  e2e       --display D [--count N]                 XTest submit → X11 client receives ButtonPress
  process   --pid PID [--seconds N]                 CPU time, wakeups (context switches) and RSS of a running engine
  dispatch                                          dyn InputBackend vs concrete call overhead

Injection commands refuse your live $DISPLAY unless --allow-live-display is given.
";

fn main() -> Result<()> {
    let all: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = all.split_first() else {
        print!("{USAGE}");
        return Ok(());
    };
    let args = Args(rest.to_vec());
    let report = match cmd.as_str() {
        "timer" => bench_timer(&args)?,
        "emit" => bench_emit(&args)?,
        "alloc" => bench_alloc(&args)?,
        "e2e" => bench_e2e(&args)?,
        "process" => bench_process(&args)?,
        "dispatch" => bench_dispatch(&args)?,
        _ => {
            print!("{USAGE}");
            return Ok(());
        }
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
