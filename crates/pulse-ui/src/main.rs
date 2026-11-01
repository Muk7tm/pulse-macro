//! `pulse-ui`: the compact Pulse window.
//!
//! The UI is a pure control client: it talks to the background engine over
//! the Unix socket from worker threads, never injects input and never takes
//! part in click timing. It starts the engine if none is running. Closing
//! the window leaves the engine (and its hotkeys) running; "Quit Pulse"
//! stops both.

mod controller;
mod form;
mod keys;
mod launcher;
mod worker;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use pulse_config::{Config, Paths, Renderer};
use slint::ComponentHandle;

/// Code generated from `ui/*.slint`. The generator's internal `unwrap`s are
/// outside this crate's lint policy.
#[allow(clippy::unwrap_used, clippy::todo, clippy::dbg_macro, clippy::all)]
mod generated {
    slint::include_modules!();
}
pub use generated::*;

const USAGE: &str = "\
Usage: pulse-ui [OPTIONS]

Options:
  --socket <PATH>       engine socket (default: $XDG_RUNTIME_DIR/pulse-macro/engine.sock)
  --config-dir <DIR>    configuration directory, read for UI preferences and
                        passed to an engine started from the UI
  -V, --version         print version
  -h, --help            print this help
";

struct Args {
    socket: Option<PathBuf>,
    config_dir: Option<PathBuf>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        socket: None,
        config_dir: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" => a.socket = Some(PathBuf::from(it.next().ok_or("--socket needs a path")?)),
            "--config-dir" => a.config_dir = Some(PathBuf::from(it.next().ok_or("--config-dir needs a path")?)),
            "-V" | "--version" => {
                println!("pulse-ui {}", env!("CARGO_PKG_VERSION"));
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

/// Pick the renderer from `ui.renderer` before any window exists.
/// `SLINT_BACKEND` still overrides the renderer when set.
fn select_backend(renderer: Renderer) {
    let base = || slint::BackendSelector::new().backend_name("winit".into());
    let selector = match renderer {
        Renderer::Auto => base(),
        Renderer::Software => base().renderer_name("software".into()),
        Renderer::Femtovg => base().renderer_name("femtovg".into()),
    };
    if let Err(e) = selector.select() {
        eprintln!("pulse-ui: renderer {renderer:?} unavailable ({e}); falling back to software rendering");
        if let Err(e) = base().renderer_name("software".into()).select() {
            eprintln!("pulse-ui: software renderer unavailable: {e}");
        }
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pulse-ui: {e}");
            return ExitCode::from(64);
        }
    };
    let paths = Paths::from_env().ok();
    let socket = match (args.socket.clone(), paths.as_ref()) {
        (Some(s), _) => s,
        (None, Some(p)) => p.socket(),
        (None, None) => {
            eprintln!("pulse-ui: XDG_RUNTIME_DIR is not set; pass --socket");
            return ExitCode::from(1);
        }
    };
    // The renderer preference is read locally (read-only) so it can be
    // applied before the window exists; the engine owns all writes.
    let config_file = args
        .config_dir
        .as_ref()
        .map(|d| d.join("config.toml"))
        .or_else(|| paths.as_ref().map(Paths::config_file));
    let renderer = config_file
        .and_then(|f| Config::load(&f).ok())
        .map_or(Renderer::Auto, |c| c.ui.renderer);
    select_backend(renderer);

    let ui = match MainWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            eprintln!("pulse-ui: cannot create the window: {e}");
            return ExitCode::from(1);
        }
    };
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
    let visible = Arc::new(AtomicBool::new(true));
    let quit = Arc::new(AtomicBool::new(false));
    let location = launcher::EngineLocation {
        socket: socket.clone(),
        custom_socket: args.socket.is_some(),
        config_dir: args.config_dir.clone(),
    };
    controller::install(controller::Controller::new(
        &ui,
        jobs_tx,
        Arc::clone(&visible),
        location,
    ));
    controller::wire(&ui);
    worker::spawn_request_worker(socket.clone(), jobs_rx);
    worker::spawn_event_watcher(socket, visible, Arc::clone(&quit));

    let result = ui.run();
    quit.store(true, std::sync::atomic::Ordering::Relaxed);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pulse-ui: {e}");
            ExitCode::from(1)
        }
    }
}
