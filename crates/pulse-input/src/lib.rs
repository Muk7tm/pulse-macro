//! Pulse input backends.
//!
//! Both real backends implement [`InputBackend`]. Only one is constructed at
//! a time by the engine; the other has no resources and runs no code.

pub mod backend;
pub mod health;
pub mod keymap;
pub mod mock;
pub mod uinput;
pub mod xtest;

pub use backend::{BackendHealth, InputBackend, InputError};
pub use health::{ProbeReport, SessionInfo, probe_uinput, probe_xtest, session_info};
pub use keymap::{KeymapMode, X11Keymap};
pub use uinput::UinputBackend;
pub use xtest::XTestBackend;

use pulse_core::BackendKind;

/// Options for constructing a backend.
#[derive(Clone, Debug, Default)]
pub struct BackendOptions {
    /// X11 display for XTest (None = `$DISPLAY`).
    pub display: Option<String>,
    /// Permit XTest on Xwayland (unsupported; for diagnostics only).
    pub allow_xwayland: bool,
}

/// Construct and fully initialize the requested backend.
pub fn open_backend(kind: BackendKind, opts: &BackendOptions) -> Result<Box<dyn InputBackend>, InputError> {
    Ok(match kind {
        BackendKind::XTest => Box::new(XTestBackend::open(opts.display.as_deref(), opts.allow_xwayland)?),
        BackendKind::UInput => Box::new(UinputBackend::open()?),
    })
}
