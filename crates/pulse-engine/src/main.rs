//! `pulse-engine`: headless macro engine. No GUI dependencies.

use std::path::PathBuf;
use std::process::ExitCode;

use pulse_config::{Config, LogLevel, Paths};
use pulse_engine::{Engine, EngineOptions, StartupError};

const USAGE: &str = "\
Usage: pulse-engine [OPTIONS]

Headless Pulse engine: hotkeys, scheduling and input injection.
Control it with pulse-cli or pulse-ui over $XDG_RUNTIME_DIR/pulse-macro/engine.sock.

Options:
  --display <DISPLAY>     X11 display (default: $DISPLAY)
  --config-dir <DIR>      configuration directory (default: $XDG_CONFIG_HOME/pulse-macro)
  --runtime-dir <DIR>     socket directory (default: $XDG_RUNTIME_DIR/pulse-macro)
  --log-level <LEVEL>     error|warn|info|debug|trace (default: from config)
  --allow-xwayland        run against Xwayland (unsupported; diagnostics only)
  -V, --version           print version
  -h, --help              print this help
";

struct Args {
    display: Option<String>,
    config_dir: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    log_level: Option<LogLevel>,
    allow_xwayland: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        display: None,
        config_dir: None,
        runtime_dir: None,
        log_level: None,
        allow_xwayland: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--display" => a.display = Some(value("--display")?),
            "--config-dir" => a.config_dir = Some(PathBuf::from(value("--config-dir")?)),
            "--runtime-dir" => a.runtime_dir = Some(PathBuf::from(value("--runtime-dir")?)),
            "--log-level" => {
                let v = value("--log-level")?;
                a.log_level = Some(match v.as_str() {
                    "error" => LogLevel::Error,
                    "warn" => LogLevel::Warn,
                    "info" => LogLevel::Info,
                    "debug" => LogLevel::Debug,
                    "trace" => LogLevel::Trace,
                    _ => return Err(format!("invalid log level `{v}`")),
                });
            }
            "--allow-xwayland" => a.allow_xwayland = true,
            "-V" | "--version" => {
                println!("pulse-engine {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            other => return Err(format!("unknown argument `{other}`\n\n{USAGE}")),
        }
    }
    Ok(Some(a))
}

fn init_logging(level: LogLevel) {
    let filter = match level {
        LogLevel::Error => tracing::Level::ERROR,
        LogLevel::Warn => tracing::Level::WARN,
        LogLevel::Info => tracing::Level::INFO,
        LogLevel::Debug => tracing::Level::DEBUG,
        LogLevel::Trace => tracing::Level::TRACE,
    };
    tracing_subscriber::fmt()
        .with_max_level(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pulse-engine: {e}");
            return ExitCode::from(64);
        }
    };
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(e) if args.config_dir.is_some() && args.runtime_dir.is_some() => {
            let _ = e;
            Paths::with_roots(PathBuf::new(), PathBuf::new())
        }
        Err(e) => {
            eprintln!("pulse-engine: {e}");
            return ExitCode::from(1);
        }
    };
    let paths = Paths {
        config_dir: args.config_dir.clone().unwrap_or(paths.config_dir),
        state_dir: paths.state_dir,
        runtime_dir: args.runtime_dir.clone().unwrap_or(paths.runtime_dir),
    };
    let level = args.log_level.unwrap_or_else(|| {
        Config::load(&paths.config_file())
            .map(|c| c.diagnostics.log_level)
            .unwrap_or(LogLevel::Warn)
    });
    init_logging(level);

    let engine = match Engine::new(EngineOptions {
        paths,
        display: args.display,
        allow_xwayland: args.allow_xwayland,
        disable_x11: false,
        backend_factory: None,
        xtest_hotkeys_are_physical: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("pulse-engine: {e}");
            return ExitCode::from(match e {
                StartupError::Ipc(pulse_engine::ipc::IpcError::AlreadyRunning(_)) => 2,
                _ => 1,
            });
        }
    };
    match engine.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pulse-engine: event loop failed: {e}");
            ExitCode::from(1)
        }
    }
}
