//! Cheap availability probes. Probing never creates virtual devices and
//! never injects input.

use std::fs::OpenOptions;
use std::path::Path;

use pulse_core::{BackendAvailability, BackendKind};
use x11rb::connection::RequestConnection;
use x11rb::protocol::xtest;

use crate::uinput::{MISSING_HINT, PERMISSION_HINT, UINPUT_PATH};
use crate::xtest::{display_name, is_xwayland};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeReport {
    pub kind: BackendKind,
    pub availability: BackendAvailability,
    pub detail: String,
    pub remediation: Option<String>,
}

/// Facts about the display session, verified against the X server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub xdg_session_type: Option<String>,
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub x11_reachable: bool,
    pub xwayland: bool,
    pub xtest: bool,
    pub xinput2: bool,
    pub vendor: Option<String>,
}

impl SessionInfo {
    pub fn native_x11(&self) -> bool {
        self.x11_reachable && !self.xwayland
    }
}

pub fn session_info(display: Option<&str>) -> SessionInfo {
    let env = |v: &str| std::env::var(v).ok().filter(|s| !s.is_empty());
    let mut info = SessionInfo {
        xdg_session_type: env("XDG_SESSION_TYPE"),
        display: display.map(str::to_owned).or_else(|| env("DISPLAY")),
        wayland_display: env("WAYLAND_DISPLAY"),
        x11_reachable: false,
        xwayland: false,
        xtest: false,
        xinput2: false,
        vendor: None,
    };
    if let Ok((conn, _)) = x11rb::connect(display) {
        use x11rb::connection::Connection;
        info.x11_reachable = true;
        info.xwayland = is_xwayland(&conn);
        info.xtest = conn
            .extension_information(xtest::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some();
        info.xinput2 = conn.extension_information("XInputExtension").ok().flatten().is_some();
        info.vendor = Some(String::from_utf8_lossy(&conn.setup().vendor).into_owned());
    }
    info
}

pub fn probe_xtest(display: Option<&str>) -> ProbeReport {
    let name = display_name(display);
    let report = |availability, detail: String, remediation: Option<&str>| ProbeReport {
        kind: BackendKind::XTest,
        availability,
        detail,
        remediation: remediation.map(str::to_owned),
    };
    let conn = match x11rb::connect(display) {
        Ok((c, _)) => c,
        Err(e) => {
            return report(
                BackendAvailability::CannotConnectX11,
                format!("cannot connect to {name}: {e}"),
                Some("start Pulse inside your X11 session (DISPLAY and XAUTHORITY must be set)"),
            );
        }
    };
    if is_xwayland(&conn) {
        return report(
            BackendAvailability::UnsupportedSession,
            format!("{name} is Xwayland"),
            Some("log in to a native X11 session; under Wayland XTest only reaches Xwayland windows"),
        );
    }
    match conn.extension_information(xtest::X11_EXTENSION_NAME) {
        Ok(Some(_)) => report(BackendAvailability::Ready, format!("XTEST available on {name}"), None),
        Ok(None) => report(
            BackendAvailability::MissingXtestExtension,
            format!("{name} has no XTEST extension"),
            Some("enable the XTEST extension in your X server configuration"),
        ),
        Err(e) => report(
            BackendAvailability::CannotConnectX11,
            format!("X11 error on {name}: {e}"),
            None,
        ),
    }
}

/// Check that `/dev/uinput` exists and is writable, without creating a device.
pub fn probe_uinput() -> ProbeReport {
    probe_uinput_at(Path::new(UINPUT_PATH))
}

pub fn probe_uinput_at(path: &Path) -> ProbeReport {
    let report = |availability, detail: String, remediation: Option<&str>| ProbeReport {
        kind: BackendKind::UInput,
        availability,
        detail,
        remediation: remediation.map(str::to_owned),
    };
    if !path.exists() {
        return report(
            BackendAvailability::DeviceMissing,
            format!("{} does not exist", path.display()),
            Some(MISSING_HINT),
        );
    }
    // Opening /dev/uinput allocates nothing until UI_DEV_CREATE.
    match OpenOptions::new().write(true).open(path) {
        Ok(_) => report(
            BackendAvailability::Ready,
            format!("{} is writable by this user", path.display()),
            None,
        ),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => report(
            BackendAvailability::DevicePermissionDenied,
            format!("permission denied opening {}", path.display()),
            Some(PERMISSION_HINT),
        ),
        Err(e) => report(
            BackendAvailability::DeviceInitializationFailed,
            format!("cannot open {}: {e}", path.display()),
            None,
        ),
    }
}
