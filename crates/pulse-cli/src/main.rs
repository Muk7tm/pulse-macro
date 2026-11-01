//! `pulse-cli`: terminal control for the Pulse engine.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use pulse_config::{Config, Paths};
use pulse_core::{ActivationMode, BackendKind, Key, MouseButton, Pressable, TargetRule};
use pulse_protocol::{Client, ClientError, Command, EngineEvent, Response, ResponseData};

#[derive(Parser)]
#[command(name = "pulse-cli", version, about = "Control the Pulse auto clicker")]
struct Cli {
    /// Engine socket (default: $XDG_RUNTIME_DIR/pulse-macro/engine.sock)
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Print raw JSON responses
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Button {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Toggle,
    Hold,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

impl OnOff {
    fn on(self) -> bool {
        matches!(self, OnOff::On)
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// State, input method, clicker settings, game window and counters
    Status,
    /// Engine and protocol versions
    Hello,
    /// Availability of each input method
    Backends,
    /// Switch the input method (xtest or uinput; only while not clicking)
    Backend { kind: String },
    /// Start clicking now
    Start,
    /// Stop clicking and release the button/key
    Stop,
    /// Stop immediately (same as the emergency-stop key)
    #[command(alias = "emergency-stop")]
    Estop,
    /// Change clicker settings (only the given options change)
    Set(SetArgs),
    #[command(subcommand)]
    Target(TargetCmd),
    /// Timing histograms, faults, process statistics
    Diagnostics {
        /// Reset counters and histograms instead
        #[arg(long)]
        reset: bool,
    },
    /// Print the configuration
    Config,
    /// Print state-change events until interrupted
    Watch,
    /// Retry the X11 connection / input method after a fault
    Reconnect,
    /// Stop everything and exit the engine
    Shutdown,
}

#[derive(clap::Args)]
struct SetArgs {
    /// Click a mouse button
    #[arg(long, value_enum, conflicts_with = "key")]
    button: Option<Button>,
    /// Press a keyboard key instead (e.g. E, Space, F, 1)
    #[arg(long)]
    key: Option<String>,
    /// Time between presses in milliseconds
    #[arg(long, conflicts_with = "cps")]
    interval: Option<u64>,
    /// Clicks per second (sets the interval to 1000/cps ms)
    #[arg(long)]
    cps: Option<f64>,
    /// Press-to-release time in milliseconds
    #[arg(long)]
    hold: Option<u64>,
    /// toggle: press the hotkey to start/stop; hold: click while held
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    /// Start/stop hotkey (e.g. F6)
    #[arg(long)]
    hotkey: Option<String>,
    /// Stop after N clicks (0 = never)
    #[arg(long)]
    count: Option<u64>,
    /// Stop after N seconds (0 = never)
    #[arg(long)]
    seconds: Option<u64>,
    /// Only click while the picked game window is in front
    #[arg(long, value_enum)]
    restrict: Option<OnOff>,
    /// Allow intervals below 10 ms
    #[arg(long, value_enum)]
    fast: Option<OnOff>,
}

#[derive(Subcommand)]
enum TargetCmd {
    /// Current game-window rule
    Show,
    /// Use the window that is in front after a countdown
    Capture {
        #[arg(long, default_value_t = 3)]
        delay: u64,
    },
    /// List top-level windows
    Windows,
    /// Set the rule explicitly
    Set {
        #[arg(long, default_value = "")]
        class: String,
        #[arg(long, default_value = "")]
        title: String,
    },
    /// Remove the rule
    Clear,
}

fn connect(cli: &Cli) -> Result<Client> {
    let path = match &cli.socket {
        Some(p) => p.clone(),
        None => Paths::from_env().context("cannot locate the engine socket")?.socket(),
    };
    let mut c = Client::connect(&path)?;
    c.set_timeout(Duration::from_secs(15));
    Ok(c)
}

fn print_json(r: &Response) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(r)?);
    Ok(())
}

fn call(c: &mut Client, json: bool, cmd: Command) -> Result<Option<ResponseData>> {
    let r = c.request(cmd)?;
    if json {
        print_json(&r)?;
    }
    if !r.ok {
        let e = r.error.unwrap_or(pulse_protocol::ErrorBody {
            code: pulse_protocol::ErrorCode::Internal,
            message: "unknown error".into(),
            issues: vec![],
        });
        for i in &e.issues {
            eprintln!("  {i}");
        }
        if e.code == pulse_protocol::ErrorCode::UnsupportedVersion {
            bail!(
                "{}. A different version of the Pulse engine is running: restart Pulse (\"Restart Pulse\" in the window, or stop the old engine and start the new one)",
                e.message
            );
        }
        bail!("{} ({:?})", e.message, e.code);
    }
    Ok(if json { None } else { r.data })
}

fn describe_clicker(c: &pulse_config::ClickerSection) -> String {
    let mode = match c.activation_mode {
        ActivationMode::Toggle => "toggle",
        ActivationMode::Hold => "hold",
    };
    let mut s = format!(
        "{} every {} ms ({:.1}/s), hold {} ms, {} on {}, emergency stop {}",
        c.target().label(),
        c.interval_ms,
        pulse_core::validation::cps(c.interval_ms),
        c.hold_ms,
        mode,
        c.activation_hotkey,
        c.emergency_stop_hotkey
    );
    if c.count > 0 {
        s.push_str(&format!(", stop after {} clicks", c.count));
    }
    if c.duration_ms > 0 {
        s.push_str(&format!(", stop after {} s", c.duration_ms / 1000));
    }
    s
}

fn print_status(s: &pulse_protocol::StatusInfo) {
    println!("state            {}", s.state);
    println!("clicker          {}", describe_clicker(&s.clicker));
    match &s.start_blocker {
        None => println!(
            "ready            yes — press {} to start/stop",
            s.clicker.activation_hotkey
        ),
        Some(b) => println!("ready            no — {b}"),
    }
    println!(
        "input method     active={} requested={}{}",
        s.active_backend.map_or("none", |b| b.id()),
        s.desired_backend.id(),
        s.backend_detail
            .as_deref()
            .map(|d| format!("  ({d})"))
            .unwrap_or_default()
    );
    let target = if s.target.is_empty() {
        "(none picked)".to_owned()
    } else {
        format!(
            "class={:?} title~{:?}",
            s.target.window_class, s.target.window_title_pattern
        )
    };
    println!(
        "game window      {target}  restriction={} in_front={}",
        if s.require_focus { "on" } else { "off" },
        s.target_focused
    );
    if let Some(w) = &s.active_window {
        println!("window in front  {} ({}) {:?}", w.class, w.instance, w.title);
    }
    for w in &s.warnings {
        println!("note             {}", w.message);
    }
    let c = &s.counters;
    println!(
        "counters         runs={} submitted_clicks={} missed_deadlines={} backend_errors={}",
        c.runs, c.submitted_clicks, c.missed_deadlines, c.backend_errors
    );
    println!("                 (submitted by Pulse; not the number of clicks the game registered)");
    if let Some(l) = &s.last_stop {
        println!(
            "last run         #{} {} after {} ms, {} clicks, {} missed, release_ok={}",
            l.run_id, l.reason, l.duration_ms, l.clicks, l.missed_deadlines, l.release_ok
        );
    }
    if let Some(e) = &s.last_error {
        println!("last message     {e}");
    }
    println!(
        "session          display={} native_x11={} hotkeys={} uptime={:.0}s",
        s.session.display.as_deref().unwrap_or("?"),
        s.session.native_x11,
        s.hotkeys_available,
        s.uptime_s
    );
}

fn apply_set(cfg: &mut Config, a: &SetArgs) -> Result<()> {
    let c = &mut cfg.clicker;
    if let Some(b) = a.button {
        c.set_target(Pressable::Button(match b {
            Button::Left => MouseButton::Left,
            Button::Right => MouseButton::Right,
            Button::Middle => MouseButton::Middle,
        }));
    }
    if let Some(k) = &a.key {
        c.set_target(Pressable::Key(Key::parse(k)?));
    }
    if let Some(i) = a.interval {
        c.interval_ms = i;
    }
    if let Some(cps) = a.cps {
        if !(cps > 0.0 && cps <= 1000.0) {
            bail!("--cps must be between 0 and 1000");
        }
        c.interval_ms = (1000.0 / cps).round().max(1.0) as u64;
    }
    if let Some(h) = a.hold {
        c.hold_ms = h;
    }
    if let Some(m) = a.mode {
        c.activation_mode = match m {
            Mode::Toggle => ActivationMode::Toggle,
            Mode::Hold => ActivationMode::Hold,
        };
    }
    if let Some(k) = &a.hotkey {
        c.activation_hotkey = Key::parse(k)?;
    }
    if let Some(n) = a.count {
        c.count = n;
    }
    if let Some(s) = a.seconds {
        c.duration_ms = s.saturating_mul(1000);
    }
    if let Some(r) = a.restrict {
        cfg.target.require_focus = r.on();
    }
    if let Some(f) = a.fast {
        cfg.engine.allow_advanced_rates = f.on();
    }
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let mut c = connect(&cli)?;
    let json = cli.json;
    match cli.cmd {
        Cmd::Status => {
            if let Some(ResponseData::Status(s)) = call(&mut c, json, Command::Status)? {
                print_status(&s);
            }
        }
        Cmd::Hello => {
            if let Some(ResponseData::Hello(h)) = call(&mut c, json, Command::Hello)? {
                println!(
                    "pulse-engine {} (protocol {}, pid {})",
                    h.engine_version, h.protocol_version, h.pid
                );
            }
        }
        Cmd::Backends => {
            if let Some(ResponseData::Backends(list)) = call(&mut c, json, Command::ListBackends)? {
                for b in list {
                    let mark = match (b.active, b.desired) {
                        (true, _) => "active",
                        (false, true) => "requested",
                        _ => "",
                    };
                    println!("{:<26} {:<28} {mark}", b.label, b.availability.label());
                    println!("    {}", b.detail);
                    if let Some(r) = b.remediation {
                        println!("    fix: {r}");
                    }
                }
            }
        }
        Cmd::Backend { kind } => {
            let Some(backend) = BackendKind::parse(&kind) else {
                bail!("unknown input method `{kind}` (use xtest or uinput)");
            };
            if let Some(ResponseData::BackendSwitched(i)) = call(&mut c, json, Command::SelectBackend { backend })? {
                println!(
                    "{:?}: active input method {}",
                    i.outcome,
                    i.active_backend.map_or("none", |b| b.id())
                );
                if let Some(d) = i.detail {
                    println!("  {d}");
                }
                for w in i.warnings {
                    println!("  warning: {w}");
                }
            }
        }
        Cmd::Start => {
            if let Some(ResponseData::Started { run_id }) = call(&mut c, json, Command::Start)? {
                println!("clicking (run {run_id})");
            }
        }
        Cmd::Stop => match call(&mut c, json, Command::Stop)? {
            Some(ResponseData::Stopped(Some(s))) => println!(
                "run {} stopped: {} ({} clicks, release_ok={})",
                s.run_id, s.reason, s.clicks, s.release_ok
            ),
            Some(ResponseData::Stopped(None)) => println!("nothing was running"),
            _ => {}
        },
        Cmd::Estop => {
            call(&mut c, json, Command::EmergencyStop)?;
            if !json {
                println!("emergency stop sent");
            }
        }
        Cmd::Set(args) => {
            let Some(ResponseData::Config(mut cfg)) = call(&mut c, false, Command::GetConfig)? else {
                bail!("engine returned no configuration");
            };
            apply_set(&mut cfg, &args)?;
            if let Some(ResponseData::Config(cfg)) = call(&mut c, json, Command::SetConfig { config: cfg })? {
                println!("clicker: {}", describe_clicker(&cfg.clicker));
                println!(
                    "restriction {}, fast speeds {}",
                    if cfg.target.require_focus { "on" } else { "off" },
                    if cfg.engine.allow_advanced_rates {
                        "allowed"
                    } else {
                        "off"
                    }
                );
            }
        }
        Cmd::Target(TargetCmd::Show) => {
            if let Some(ResponseData::Status(s)) = call(&mut c, json, Command::Status)? {
                println!(
                    "class={:?} title~{:?} restriction={} in_front={}",
                    s.target.window_class, s.target.window_title_pattern, s.require_focus, s.target_focused
                );
            }
        }
        Cmd::Target(TargetCmd::Capture { delay }) => {
            for i in (1..=delay).rev() {
                eprintln!("switch to the game window... {i}");
                std::thread::sleep(Duration::from_secs(1));
            }
            let Some(ResponseData::Window(Some(w))) = call(&mut c, false, Command::ActiveWindow)? else {
                bail!("no window is reported as active (does your window manager set _NET_ACTIVE_WINDOW?)");
            };
            let rule = TargetRule::from_window(&w);
            println!(
                "picked window {:#x}: class={:?} instance={:?} title={:?}",
                w.id, w.class, w.instance, w.title
            );
            call(&mut c, json, Command::SetTarget { rule: rule.clone() })?;
            println!(
                "game window rule: class={:?} title~{:?}",
                rule.window_class, rule.window_title_pattern
            );
        }
        Cmd::Target(TargetCmd::Windows) => {
            if let Some(ResponseData::Windows(list)) = call(&mut c, json, Command::ListWindows)? {
                for w in list {
                    println!("{:#010x}  {:<28} {:<20} {}", w.id, w.class, w.instance, w.title);
                }
            }
        }
        Cmd::Target(TargetCmd::Set { class, title }) => {
            call(
                &mut c,
                json,
                Command::SetTarget {
                    rule: TargetRule {
                        window_class: class,
                        window_title_pattern: title,
                    },
                },
            )?;
        }
        Cmd::Target(TargetCmd::Clear) => {
            call(
                &mut c,
                json,
                Command::SetTarget {
                    rule: TargetRule::default(),
                },
            )?;
        }
        Cmd::Diagnostics { reset: true } => {
            call(&mut c, json, Command::ResetDiagnostics)?;
        }
        Cmd::Diagnostics { reset: false } => {
            if let Some(ResponseData::Diagnostics(d)) = call(&mut c, json, Command::Diagnostics)? {
                let p = &d.process;
                println!(
                    "process   pid {} rss {} KiB threads {} cpu user {:.2}s sys {:.2}s ctx-switches vol {} invol {}",
                    p.pid,
                    p.rss_kib,
                    p.threads,
                    p.cpu_user_s,
                    p.cpu_system_s,
                    p.voluntary_ctx_switches,
                    p.involuntary_ctx_switches
                );
                println!(
                    "{:<16} {:>8} {:>10} {:>10} {:>10} {:>10} {:>10}",
                    "metric (µs)", "count", "p50", "p95", "p99", "max", "mean"
                );
                for h in &d.histograms {
                    println!(
                        "{:<16} {:>8} {:>10.1} {:>10.1} {:>10.1} {:>10.1} {:>10.1}",
                        h.name, h.count, h.p50_us, h.p95_us, h.p99_us, h.max_us, h.mean_us
                    );
                }
                println!("(percentiles are histogram bucket upper bounds, ≤12.5% resolution)");
                if let Some(k) = &d.keymap {
                    println!("keymap    {k}");
                }
                println!("synthetic devices filtered from hotkeys: {:?}", d.synthetic_devices);
                for f in &d.faults {
                    println!("fault  +{}s  {}", f.uptime_s, f.message);
                }
            }
        }
        Cmd::Config => {
            if let Some(ResponseData::Config(cfg)) = call(&mut c, json, Command::GetConfig)? {
                print!("{}", cfg.to_toml_string()?);
            }
        }
        Cmd::Watch => {
            call(&mut c, false, Command::Subscribe)?;
            eprintln!("watching engine events (Ctrl+C to quit)");
            loop {
                match c.next_event(None) {
                    Ok(Some(e)) => {
                        if json {
                            println!("{}", serde_json::to_string(&e)?);
                        } else {
                            match e {
                                EngineEvent::StateChanged { state, reason, run_id } => println!(
                                    "state {state}{}{}",
                                    run_id.map(|r| format!(" run={r}")).unwrap_or_default(),
                                    reason.map(|r| format!(" ({r})")).unwrap_or_default()
                                ),
                                EngineEvent::TargetFocus { focused, window } => println!(
                                    "game window in front={focused} window={}",
                                    window.map(|w| w.class).unwrap_or_default()
                                ),
                                EngineEvent::Fault { message } => println!("message {message}"),
                                EngineEvent::ConfigChanged => println!("settings changed"),
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(ClientError::Closed) => bail!("engine closed the connection"),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Cmd::Reconnect => {
            call(&mut c, json, Command::Reconnect)?;
        }
        Cmd::Shutdown => {
            call(&mut c, json, Command::Shutdown)?;
            if !json {
                println!("Pulse is shutting down");
            }
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pulse-cli: {e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> SetArgs {
        let mut full = vec!["pulse-cli", "set"];
        full.extend_from_slice(list);
        match Cli::try_parse_from(full).expect("parse").cmd {
            Cmd::Set(a) => a,
            _ => panic!("not set"),
        }
    }

    #[test]
    fn set_maps_options_onto_config() {
        let mut cfg = Config::default();
        apply_set(
            &mut cfg,
            &args(&[
                "--key",
                "space",
                "--cps",
                "20",
                "--hold",
                "15",
                "--mode",
                "hold",
                "--restrict",
                "off",
            ]),
        )
        .expect("apply");
        assert_eq!(cfg.clicker.target(), Pressable::Key(Key::parse("Space").expect("key")));
        assert_eq!(cfg.clicker.interval_ms, 50);
        assert_eq!(cfg.clicker.hold_ms, 15);
        assert_eq!(cfg.clicker.activation_mode, ActivationMode::Hold);
        assert!(!cfg.target.require_focus);
        apply_set(&mut cfg, &args(&["--button", "right", "--seconds", "30"])).expect("apply");
        assert_eq!(cfg.clicker.target(), Pressable::Button(MouseButton::Right));
        assert_eq!(cfg.clicker.duration_ms, 30_000);
    }

    #[test]
    fn set_rejects_conflicts_and_bad_values() {
        let full = ["pulse-cli", "set", "--button", "left", "--key", "E"];
        assert!(Cli::try_parse_from(full).is_err());
        let mut cfg = Config::default();
        assert!(apply_set(&mut cfg, &args(&["--key", "NotAKey"])).is_err());
        assert!(apply_set(&mut cfg, &args(&["--cps", "0"])).is_err());
    }
}
