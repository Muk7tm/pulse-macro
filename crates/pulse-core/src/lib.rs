//! Pulse core: GUI-free, OS-free logic shared by the engine, CLI and UI.
//!
//! Nothing in this crate performs I/O or reads clocks. That keeps the
//! clicker, scheduler, trigger and focus logic deterministic and testable
//! with a fake clock.

pub mod clicker;
pub mod errors;
pub mod focus;
pub mod held_inputs;
pub mod hotkeys;
pub mod input;
pub mod scheduler;
pub mod state;
pub mod time;
pub mod validation;

pub use clicker::{ClickerSpec, Compiled, Program};
pub use errors::{Issue, Severity, ValidationFailed};
pub use focus::{FocusGuard, TargetRule, WindowInfo};
pub use held_inputs::HeldInputs;
pub use hotkeys::{Hotkey, SideButton};
pub use input::{Key, KeyState, LogicalInput, MouseButton, Pressable};
pub use scheduler::{Executor, FinishReason, Next, RunStats, Step};
pub use state::{ActivationMode, EngineState, StopReason};
pub use time::Nanos;
pub use validation::Limits;

/// Input backend identifiers. Lives in core so configuration, protocol and
/// UI can name a backend without linking any backend implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum BackendKind {
    #[serde(rename = "xtest")]
    XTest,
    #[serde(rename = "uinput")]
    UInput,
}

impl BackendKind {
    pub const ALL: [BackendKind; 2] = [BackendKind::XTest, BackendKind::UInput];

    pub const fn id(self) -> &'static str {
        match self {
            BackendKind::XTest => "xtest",
            BackendKind::UInput => "uinput",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            BackendKind::XTest => "XTest (X11)",
            BackendKind::UInput => "uinput (Virtual Device)",
        }
    }

    pub fn parse(s: &str) -> Option<BackendKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "xtest" | "x11" => Some(BackendKind::XTest),
            "uinput" => Some(BackendKind::UInput),
            _ => None,
        }
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// Result of probing or initializing a backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendAvailability {
    Ready,
    MissingXtestExtension,
    CannotConnectX11,
    DevicePermissionDenied,
    DeviceMissing,
    DeviceInitializationFailed,
    UnsupportedSession,
    Unknown,
}

impl BackendAvailability {
    pub const fn label(self) -> &'static str {
        match self {
            BackendAvailability::Ready => "Ready",
            BackendAvailability::MissingXtestExtension => "Missing XTest Extension",
            BackendAvailability::CannotConnectX11 => "Cannot Connect to X11",
            BackendAvailability::DevicePermissionDenied => "Device Permission Denied",
            BackendAvailability::DeviceMissing => "Device Missing",
            BackendAvailability::DeviceInitializationFailed => "Device Initialization Failed",
            BackendAvailability::UnsupportedSession => "Unsupported Session",
            BackendAvailability::Unknown => "Unknown",
        }
    }
}
