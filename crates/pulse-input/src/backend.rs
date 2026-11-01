//! The input backend contract shared by XTest and uinput.
//!
//! Construction *is* initialization: `XTestBackend::open` and
//! `UinputBackend::open` either return a fully working backend or an error,
//! so a half-initialized backend can never be reported `Ready`.

use std::path::PathBuf;

use pulse_core::{BackendAvailability, BackendKind, HeldInputs, LogicalInput};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InputError {
    #[error("cannot connect to X11 display {display}: {reason}")]
    CannotConnectX11 { display: String, reason: String },
    #[error("the X server on {display} does not provide the XTEST extension")]
    MissingXTest { display: String },
    #[error("unsupported session: {0}")]
    UnsupportedSession(String),
    #[error("permission denied opening {path}: {hint}")]
    DevicePermissionDenied { path: PathBuf, hint: String },
    #[error("{path} does not exist: {hint}")]
    DeviceMissing { path: PathBuf, hint: String },
    #[error("virtual device initialization failed: {0}")]
    DeviceInit(String),
    #[error("input submission failed: {0}")]
    Submit(String),
    #[error("backend disconnected: {0}")]
    Disconnected(String),
    #[error("unsupported input: {0}")]
    Unsupported(String),
}

impl InputError {
    pub fn availability(&self) -> BackendAvailability {
        match self {
            InputError::CannotConnectX11 { .. } => BackendAvailability::CannotConnectX11,
            InputError::MissingXTest { .. } => BackendAvailability::MissingXtestExtension,
            InputError::UnsupportedSession(_) => BackendAvailability::UnsupportedSession,
            InputError::DevicePermissionDenied { .. } => BackendAvailability::DevicePermissionDenied,
            InputError::DeviceMissing { .. } => BackendAvailability::DeviceMissing,
            InputError::DeviceInit(_) => BackendAvailability::DeviceInitializationFailed,
            InputError::Submit(_) | InputError::Disconnected(_) | InputError::Unsupported(_) => {
                BackendAvailability::Unknown
            }
        }
    }

    /// Whether the backend must be considered dead after this error.
    pub fn is_fatal(&self) -> bool {
        !matches!(self, InputError::Unsupported(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendHealth {
    pub kind: BackendKind,
    pub healthy: bool,
    /// Human-readable details: display/extension version or device nodes.
    pub detail: String,
    pub last_error: Option<String>,
}

pub trait InputBackend: Send {
    fn kind(&self) -> BackendKind;

    /// Submit one batch of semantically simultaneous, prevalidated events.
    /// Success means *submitted locally*, not processed by the game.
    fn emit(&mut self, events: &[LogicalInput]) -> Result<(), InputError>;

    /// Release everything this backend believes it holds plus `extra`
    /// (the engine ledger). Never presses anything. Best effort: attempts
    /// every release even if one fails, then reports the first error.
    fn release_all(&mut self, extra: &HeldInputs) -> Result<(), InputError>;

    /// Inputs this backend currently holds according to its own ledger.
    fn held(&self) -> HeldInputs;

    /// Off-hot-path liveness check: drains asynchronous errors (X11) or
    /// verifies the device handle (uinput).
    fn check(&mut self) -> Result<(), InputError>;

    /// Verify that every key/button in `set` can be emitted.
    fn supports(&self, set: &HeldInputs) -> Result<(), InputError>;

    fn health(&self) -> BackendHealth;
}
