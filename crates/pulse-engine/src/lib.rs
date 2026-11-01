//! Pulse headless engine. The binary is a thin wrapper around [`Engine`];
//! the library form exists so integration tests can run an engine in-process
//! with temporary directories and mock backends.

pub mod clock;
pub mod ipc;
pub mod metrics;
pub mod runner;
pub mod service;
pub mod x11_watch;

pub use service::{BackendFactory, Engine, EngineOptions, StartupError};
