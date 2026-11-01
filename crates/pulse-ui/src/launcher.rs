//! Starting, probing and restarting the background Pulse engine.
//!
//! The UI starts the engine automatically when none is running. The engine
//! is fully detached (own process group, no shared terminal), so closing the
//! window never stops it. An engine from an older Pulse version speaks a
//! different protocol; the UI detects that with a raw probe that does not
//! depend on message shapes, and can restart it on request.

use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use pulse_protocol::{FrameDecoder, PROTOCOL_VERSION, encode_frame};

/// How the UI was told to reach the engine.
#[derive(Clone, Debug)]
pub struct EngineLocation {
    pub socket: PathBuf,
    /// `--socket` was given: pass its directory to a spawned engine.
    pub custom_socket: bool,
    pub config_dir: Option<PathBuf>,
}

impl EngineLocation {
    pub fn lock_file(&self) -> PathBuf {
        self.socket.with_file_name("engine.lock")
    }

    /// Where a UI-started engine writes its log.
    fn log_file(&self) -> Option<PathBuf> {
        match &self.config_dir {
            Some(dir) => Some(dir.join("engine.log")),
            None => pulse_config::Paths::from_env()
                .ok()
                .map(|p| p.state_dir.join("engine.log")),
        }
    }
}

fn engine_executable() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("pulse-engine")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("pulse-engine"))
}

/// Start `pulse-engine` detached. Its output goes to `engine.log` in the
/// state directory (or nowhere if that cannot be opened).
pub fn spawn_engine(loc: &EngineLocation) -> Result<(), String> {
    let exe = engine_executable();
    let mut cmd = std::process::Command::new(&exe);
    if loc.custom_socket {
        if let Some(dir) = loc.socket.parent() {
            cmd.arg("--runtime-dir").arg(dir);
        }
    }
    if let Some(dir) = &loc.config_dir {
        cmd.arg("--config-dir").arg(dir);
    }
    let log = loc.log_file().and_then(|path| {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
    });
    let stderr = match log {
        Some(f) => std::process::Stdio::from(f),
        None => std::process::Stdio::null(),
    };
    // Own process group and no shared stdio: closing the UI (or its
    // terminal) never signals the engine.
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(stderr)
        .process_group(0);
    match cmd.spawn() {
        Ok(mut child) => {
            // Reap the child if it exits while the UI is still open.
            let _ = std::thread::Builder::new()
                .name("pulse-ui-reaper".into())
                .spawn(move || drop(child.wait()));
            Ok(())
        }
        Err(e) => Err(format!("cannot start {}: {e}", exe.display())),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Nothing is listening (no socket, or connection refused).
    NotRunning,
    /// An engine answered with this protocol version.
    Version(u32),
    /// Something answered, but not understandably.
    Failed(String),
}

/// Ask the engine for its protocol version with a raw `hello`, without
/// assuming anything else about its message format.
pub fn probe(socket: &Path) -> Probe {
    let mut stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Probe::NotRunning;
        }
        Err(e) => return Probe::Failed(e.to_string()),
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let request = serde_json::json!({
        "protocol_version": PROTOCOL_VERSION,
        "request_id": "ui-probe",
        "command": "hello",
    });
    let frame = match encode_frame(&request) {
        Ok(f) => f,
        Err(e) => return Probe::Failed(e.to_string()),
    };
    if let Err(e) = stream.write_all(&frame) {
        return Probe::Failed(e.to_string());
    }
    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        match decoder.next_frame() {
            Ok(Some(body)) => return version_of(&body),
            Ok(None) => {}
            Err(e) => return Probe::Failed(e.to_string()),
        }
        match stream.read(&mut buf) {
            Ok(0) => return Probe::Failed("the engine closed the connection".into()),
            Ok(n) => decoder.push(&buf[..n]),
            Err(e) => return Probe::Failed(e.to_string()),
        }
    }
}

fn version_of(body: &[u8]) -> Probe {
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(v) => match v.get("protocol_version").and_then(serde_json::Value::as_u64) {
            Some(n) => Probe::Version(u32::try_from(n).unwrap_or(u32::MAX)),
            None => Probe::Failed("reply without a protocol version".into()),
        },
        Err(e) => Probe::Failed(e.to_string()),
    }
}

/// PID written by the engine into its single-instance lock file.
pub fn parse_lock_pid(text: &str) -> Option<i32> {
    text.trim().parse::<i32>().ok().filter(|p| *p > 1)
}

/// Check that `pid` is a `pulse-engine` process owned by this user.
pub fn verify_engine_process(pid: i32, proc_root: &Path) -> Result<(), String> {
    let dir = proc_root.join(pid.to_string());
    let comm = std::fs::read_to_string(dir.join("comm")).map_err(|_| format!("process {pid} is not running"))?;
    if comm.trim() != "pulse-engine" {
        return Err(format!("process {pid} is “{}”, not pulse-engine", comm.trim()));
    }
    let owner = std::fs::metadata(&dir).map_err(|e| e.to_string())?.uid();
    let me = rustix::process::geteuid().as_raw();
    if owner != me {
        return Err(format!("process {pid} belongs to another user"));
    }
    Ok(())
}

/// Ask an engine speaking `version` of the protocol to shut down.
fn raw_shutdown(socket: &Path, version: u32) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    let request = serde_json::json!({
        "protocol_version": version,
        "request_id": "ui-restart",
        "command": "shutdown",
    });
    let frame = encode_frame(&request).map_err(|e| e.to_string())?;
    stream.write_all(&frame).map_err(|e| e.to_string())
}

/// Stop the older engine: SIGTERM to the PID in its lock file (it then
/// releases any held input and removes its socket), falling back to a
/// shutdown request in its own protocol version.
pub fn stop_old_engine(loc: &EngineLocation, version: Option<u32>) -> Result<(), String> {
    let pid = std::fs::read_to_string(loc.lock_file())
        .ok()
        .and_then(|t| parse_lock_pid(&t));
    let by_signal = pid.ok_or_else(|| "no PID in the lock file".to_owned()).and_then(|pid| {
        verify_engine_process(pid, Path::new("/proc"))?;
        let p = rustix::process::Pid::from_raw(pid).ok_or("invalid PID")?;
        rustix::process::kill_process(p, rustix::process::Signal::TERM).map_err(|e| e.to_string())
    });
    match (by_signal, version) {
        (Ok(()), _) => Ok(()),
        (Err(_), Some(v)) => raw_shutdown(&loc.socket, v),
        (Err(e), None) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_pid_parsing() {
        assert_eq!(parse_lock_pid("3160\n"), Some(3160));
        assert_eq!(parse_lock_pid("  42 "), Some(42));
        assert_eq!(parse_lock_pid(""), None);
        assert_eq!(parse_lock_pid("1"), None, "never signal init");
        assert_eq!(parse_lock_pid("-5"), None);
        assert_eq!(parse_lock_pid("abc"), None);
    }

    #[test]
    fn process_verification_checks_name() {
        let root = std::env::temp_dir().join(format!("pulse-ui-proc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("100")).expect("mkdir");
        std::fs::write(root.join("100/comm"), "pulse-engine\n").expect("write");
        std::fs::create_dir_all(root.join("200")).expect("mkdir");
        std::fs::write(root.join("200/comm"), "bash\n").expect("write");
        assert!(verify_engine_process(100, &root).is_ok());
        assert!(
            verify_engine_process(200, &root)
                .expect_err("not engine")
                .contains("bash")
        );
        assert!(verify_engine_process(300, &root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn version_parsing_ignores_message_shape() {
        assert_eq!(
            version_of(br#"{"type":"response","protocol_version":1,"state":"armed"}"#),
            Probe::Version(1)
        );
        assert_eq!(version_of(br#"{"protocol_version":2}"#), Probe::Version(2));
        assert!(matches!(version_of(b"{}"), Probe::Failed(_)));
        assert!(matches!(version_of(b"not json"), Probe::Failed(_)));
    }

    #[test]
    fn probe_reports_not_running() {
        let p = std::env::temp_dir().join(format!("pulse-ui-nosock-{}", std::process::id()));
        assert_eq!(probe(&p), Probe::NotRunning);
    }
}
