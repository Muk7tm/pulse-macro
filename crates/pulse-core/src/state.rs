//! Engine state machine.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineState {
    /// Validating config and dependencies.
    Booting,
    /// No input backend initialized; nothing can be emitted.
    Idle,
    /// Backend initialized; the hotkey starts the clicker.
    Ready,
    /// Clicking.
    Running,
    /// Rejecting new events and releasing held input.
    Stopping,
    /// Fault requiring user attention.
    Error,
    /// Closing devices and IPC.
    ShuttingDown,
}

impl EngineState {
    pub const fn name(self) -> &'static str {
        match self {
            EngineState::Booting => "booting",
            EngineState::Idle => "idle",
            EngineState::Ready => "ready",
            EngineState::Running => "running",
            EngineState::Stopping => "stopping",
            EngineState::Error => "error",
            EngineState::ShuttingDown => "shutting_down",
        }
    }

    /// Backend switching and configuration are allowed only in safe states.
    pub const fn is_safe_for_reconfigure(self) -> bool {
        matches!(self, EngineState::Idle | EngineState::Ready | EngineState::Error)
    }

    pub const fn is_active(self) -> bool {
        matches!(self, EngineState::Running | EngineState::Stopping)
    }

    /// Whether `self → to` is a legal transition.
    pub fn can_transition(self, to: EngineState) -> bool {
        use EngineState::*;
        if to == ShuttingDown {
            return self != ShuttingDown;
        }
        match self {
            Booting => matches!(to, Idle | Ready | Error),
            Idle => matches!(to, Ready | Error),
            Ready => matches!(to, Idle | Running | Error),
            Running => matches!(to, Stopping | Error),
            Stopping => matches!(to, Idle | Ready | Error),
            Error => matches!(to, Idle | Ready),
            ShuttingDown => false,
        }
    }
}

impl fmt::Display for EngineState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a run ended. Logged and reported with every stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    UserStop,
    EmergencyStop,
    TriggerReleased,
    Completed,
    DurationElapsed,
    FocusLost,
    Suspend,
    BackendFault,
    X11ConnectionLost,
    Shutdown,
}

impl StopReason {
    pub const ALL: [StopReason; 10] = [
        StopReason::UserStop,
        StopReason::EmergencyStop,
        StopReason::TriggerReleased,
        StopReason::Completed,
        StopReason::DurationElapsed,
        StopReason::FocusLost,
        StopReason::Suspend,
        StopReason::BackendFault,
        StopReason::X11ConnectionLost,
        StopReason::Shutdown,
    ];

    pub const fn describe(self) -> &'static str {
        match self {
            StopReason::UserStop => "stopped by user",
            StopReason::EmergencyStop => "emergency stop",
            StopReason::TriggerReleased => "hotkey released",
            StopReason::Completed => "click limit reached",
            StopReason::DurationElapsed => "time limit reached",
            StopReason::FocusLost => "game window lost focus",
            StopReason::Suspend => "stopped after suspend/resume",
            StopReason::BackendFault => "input backend fault",
            StopReason::X11ConnectionLost => "X11 connection lost",
            StopReason::Shutdown => "Pulse shutting down",
        }
    }
}

impl fmt::Display for StopReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.describe())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationMode {
    /// Hotkey press toggles clicking; release has no effect.
    Toggle,
    /// Clicks while the hotkey is physically held.
    Hold,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_cannot_jump_to_ready() {
        assert!(!EngineState::Running.can_transition(EngineState::Ready));
        assert!(EngineState::Running.can_transition(EngineState::Stopping));
        assert!(EngineState::Stopping.can_transition(EngineState::Ready));
    }

    #[test]
    fn error_and_idle_never_go_directly_to_running() {
        assert!(!EngineState::Error.can_transition(EngineState::Running));
        assert!(!EngineState::Idle.can_transition(EngineState::Running));
    }

    #[test]
    fn reconfigure_only_when_safe() {
        assert!(!EngineState::Running.is_safe_for_reconfigure());
        assert!(!EngineState::Stopping.is_safe_for_reconfigure());
        assert!(EngineState::Ready.is_safe_for_reconfigure());
    }

    #[test]
    fn all_reasons_listed_once() {
        for (i, a) in StopReason::ALL.iter().enumerate() {
            assert!(!StopReason::ALL[i + 1..].contains(a));
        }
    }

    #[test]
    fn serde_names() {
        assert_eq!(
            serde_json::to_string(&EngineState::ShuttingDown).ok().as_deref(),
            Some("\"shutting_down\"")
        );
        assert_eq!(
            serde_json::to_string(&StopReason::FocusLost).ok().as_deref(),
            Some("\"focus_lost\"")
        );
    }
}
