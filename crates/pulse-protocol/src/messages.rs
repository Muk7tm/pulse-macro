//! Request, response and event types.

use pulse_config::{ClickerSection, Config};
use pulse_core::{BackendAvailability, BackendKind, EngineState, Issue, StopReason, TargetRule, WindowInfo};
use serde::{Deserialize, Serialize};

use crate::PROTOCOL_VERSION;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(flatten)]
    pub command: Command,
}

impl Request {
    pub fn new(request_id: impl Into<String>, command: Command) -> Request {
        Request {
            protocol_version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            command,
        }
    }
}

/// Control commands. Every command is idempotent or explicitly retry-safe.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", content = "payload", rename_all = "snake_case")]
pub enum Command {
    /// Engine and protocol versions.
    Hello,
    /// State, input method, clicker settings, target, counters.
    Status,
    /// Capability and readiness of each input method (no device creation).
    ListBackends,
    /// Switch the input method transactionally; only while not clicking.
    SelectBackend {
        backend: BackendKind,
    },
    GetConfig,
    /// Replace the configuration (validated, saved atomically, applied
    /// immediately). `engine.backend` is ignored: use `select_backend`.
    SetConfig {
        config: Box<Config>,
    },
    /// Start clicking now (as if the hotkey were pressed).
    Start,
    Stop,
    /// Stop immediately and release everything.
    EmergencyStop,
    /// Set the game-window rule (saved).
    SetTarget {
        rule: TargetRule,
    },
    /// Metadata of the currently active X11 window.
    ActiveWindow,
    /// Top-level client windows (`_NET_CLIENT_LIST`).
    ListWindows,
    Diagnostics,
    ResetDiagnostics,
    /// Receive state-change events on this connection.
    Subscribe,
    /// Retry X11 connection and the input method after a fault.
    Reconnect,
    /// Stop everything and exit the engine.
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    UnsupportedVersion,
    Unauthorized,
    /// The command is not allowed in the current state.
    InvalidState,
    ValidationFailed,
    NotFound,
    BackendUnavailable,
    /// The X11 connection required for hotkeys/focus is unavailable.
    X11Unavailable,
    Io,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<Issue>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub protocol_version: u32,
    pub request_id: String,
    pub ok: bool,
    pub state: EngineState,
    pub active_backend: Option<BackendKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<ResponseData>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ResponseData {
    Hello(HelloInfo),
    Status(Box<StatusInfo>),
    Backends(Vec<BackendInfo>),
    Config(Box<Config>),
    Started { run_id: u64 },
    Stopped(Option<StopInfo>),
    BackendSwitched(BackendSwitchInfo),
    Window(Option<WindowInfo>),
    Windows(Vec<WindowInfo>),
    Diagnostics(Box<DiagnosticsInfo>),
    Ack,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloInfo {
    pub engine_version: String,
    pub protocol_version: u32,
    pub pid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopInfo {
    pub run_id: u64,
    pub reason: StopReason,
    pub clicks: u64,
    pub events: u64,
    pub missed_deadlines: u64,
    pub duration_ms: u64,
    /// False when releasing held input failed (input state may be stuck).
    pub release_ok: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    pub runs: u64,
    /// Submitted locally to the input method — not clicks registered by the game.
    pub submitted_clicks: u64,
    pub submitted_events: u64,
    pub missed_deadlines: u64,
    pub current_run_clicks: u64,
    pub backend_errors: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub display: Option<String>,
    pub xdg_session_type: Option<String>,
    pub native_x11: bool,
    pub xwayland: bool,
    pub xtest: bool,
    pub xinput2: bool,
    pub vendor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusInfo {
    pub engine_version: String,
    pub state: EngineState,
    /// Requested/persisted input method.
    pub desired_backend: BackendKind,
    /// Initialized input method, if any.
    pub active_backend: Option<BackendKind>,
    pub backend_detail: Option<String>,
    /// The clicker settings in effect.
    pub clicker: ClickerSection,
    /// Whether the hotkey can start clicking right now (ignoring which
    /// window is in front, which changes constantly).
    pub can_start: bool,
    /// Why clicking cannot start, in plain language (e.g. "Pick the game
    /// window first").
    pub start_blocker: Option<String>,
    /// Non-blocking notes about the settings (e.g. very fast intervals).
    #[serde(default)]
    pub warnings: Vec<Issue>,
    pub run_id: Option<u64>,
    pub last_stop: Option<StopInfo>,
    pub last_error: Option<String>,
    pub require_focus: bool,
    pub target: TargetRule,
    pub target_focused: bool,
    pub active_window: Option<WindowInfo>,
    pub hotkeys_available: bool,
    pub counters: Counters,
    pub session: SessionSummary,
    pub uptime_s: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub kind: BackendKind,
    pub label: String,
    pub description: String,
    pub availability: BackendAvailability,
    pub detail: String,
    pub remediation: Option<String>,
    pub desired: bool,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchOutcome {
    Switched,
    AlreadyActive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSwitchInfo {
    pub outcome: SwitchOutcome,
    pub active_backend: Option<BackendKind>,
    pub detail: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistogramSummary {
    pub name: String,
    pub description: String,
    pub count: u64,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
    pub mean_us: f64,
    /// Non-empty buckets: (upper bound in µs, count).
    pub buckets: Vec<(f64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultEntry {
    pub uptime_s: u64,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProcessStats {
    pub pid: u32,
    pub rss_kib: u64,
    pub cpu_user_s: f64,
    pub cpu_system_s: f64,
    pub threads: u64,
    pub voluntary_ctx_switches: u64,
    pub involuntary_ctx_switches: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticsInfo {
    pub histograms: Vec<HistogramSummary>,
    pub faults: Vec<FaultEntry>,
    pub process: ProcessStats,
    pub session: SessionSummary,
    pub backends: Vec<BackendInfo>,
    pub keymap: Option<String>,
    pub synthetic_devices: Vec<String>,
}

/// Low-rate notifications for subscribed clients. Never per click.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    StateChanged {
        state: EngineState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run_id: Option<u64>,
    },
    TargetFocus {
        focused: bool,
        window: Option<WindowInfo>,
    },
    Fault {
        message: String,
    },
    ConfigChanged,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EventMessage {
    pub protocol_version: u32,
    pub event: EngineEvent,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Response(Response),
    Event(EventMessage),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        let r = Request::new(
            "a1b2c3",
            Command::SelectBackend {
                backend: BackendKind::UInput,
            },
        );
        let v: serde_json::Value = serde_json::to_value(&r).expect("ser");
        assert_eq!(
            v,
            serde_json::json!({
                "protocol_version": 2,
                "request_id": "a1b2c3",
                "command": "select_backend",
                "payload": {"backend": "uinput"}
            })
        );
        let back: Request = serde_json::from_value(v).expect("de");
        assert_eq!(back, r);
    }

    #[test]
    fn unit_commands_need_no_payload() {
        let r: Request =
            serde_json::from_str(r#"{"protocol_version":2,"request_id":"x","command":"status"}"#).expect("de");
        assert_eq!(r.command, Command::Status);
    }

    #[test]
    fn unknown_commands_are_rejected() {
        for cmd in ["exec", "shell", "run_script", "load_plugin"] {
            let text = format!(r#"{{"protocol_version":2,"request_id":"x","command":"{cmd}"}}"#);
            assert!(serde_json::from_str::<Request>(&text).is_err(), "{cmd}");
        }
    }

    #[test]
    fn response_wire_format() {
        let resp = ServerMessage::Response(Response {
            protocol_version: 2,
            request_id: "a1b2c3".into(),
            ok: true,
            state: EngineState::Ready,
            active_backend: Some(BackendKind::UInput),
            error: None,
            data: None,
        });
        let v = serde_json::to_value(&resp).expect("ser");
        assert_eq!(
            v,
            serde_json::json!({
                "type": "response",
                "protocol_version": 2,
                "request_id": "a1b2c3",
                "ok": true,
                "state": "ready",
                "active_backend": "uinput"
            })
        );
        let back: ServerMessage = serde_json::from_value(v).expect("de");
        assert_eq!(back, resp);
    }

    #[test]
    fn data_variants_roundtrip() {
        for data in [
            ResponseData::Started { run_id: 7 },
            ResponseData::Windows(vec![WindowInfo::default()]),
            ResponseData::Ack,
            ResponseData::Stopped(None),
        ] {
            let s = serde_json::to_string(&data).expect("ser");
            let back: ResponseData = serde_json::from_str(&s).expect("de");
            assert_eq!(back, data);
        }
    }
}
