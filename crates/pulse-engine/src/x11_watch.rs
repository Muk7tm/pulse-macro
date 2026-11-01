//! X11 observation on the control thread: global hotkeys via
//! XInput2 raw key events and focus tracking via `_NET_ACTIVE_WINDOW`.
//!
//! Raw events are observed passively (no grab), so the hotkey still reaches
//! the focused application. Events whose source device is Pulse's own
//! injection path (the XTEST slave devices or the Pulse uinput devices) are
//! dropped, so Pulse can never trigger itself. Everything is event-driven;
//! nothing here polls.

use std::os::fd::{AsFd, BorrowedFd};

use pulse_core::{Key, Nanos, WindowInfo};
use pulse_input::X11Keymap;
use pulse_input::uinput::{KEYBOARD_NAME, MOUSE_NAME};
use pulse_protocol::SessionSummary;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::cookie::Cookie;
use x11rb::protocol::Event;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, EventMask, GetPropertyReply,
};
use x11rb::rust_connection::RustConnection;

use crate::clock::monotonic_now;

#[derive(Debug)]
pub enum X11Event {
    Key {
        key: Key,
        pressed: bool,
        received_at: Nanos,
    },
    FocusChanged(Option<WindowInfo>),
    DevicesChanged,
}

struct Atoms {
    net_active_window: u32,
    net_wm_name: u32,
    net_wm_pid: u32,
    net_client_list: u32,
    utf8_string: u32,
}

pub struct X11Watch {
    conn: RustConnection,
    root: u32,
    display: String,
    keymap: X11Keymap,
    synthetic: Vec<u16>,
    device_names: Vec<String>,
    atoms: Atoms,
    watched: Option<u32>,
    ewmh: bool,
    /// Test-only: accept XTEST events as physical (Xvfb has no other keyboard).
    xtest_is_physical: bool,
    pub session: SessionSummary,
}

fn intern(conn: &RustConnection, name: &str) -> Result<u32, String> {
    conn.intern_atom(false, name.as_bytes())
        .map_err(|e| e.to_string())?
        .reply()
        .map(|r| r.atom)
        .map_err(|e| e.to_string())
}

fn prop_text(r: &GetPropertyReply) -> String {
    String::from_utf8_lossy(&r.value).trim_end_matches('\0').to_owned()
}

impl X11Watch {
    pub fn connect(display: Option<&str>, xtest_is_physical: bool) -> Result<X11Watch, String> {
        let name = pulse_input::xtest::display_name(display);
        let (conn, screen) = x11rb::connect(display).map_err(|e| format!("cannot connect to {name}: {e}"))?;
        let root = conn.setup().roots[screen].root;
        if conn
            .extension_information(xinput::X11_EXTENSION_NAME)
            .map_err(|e| e.to_string())?
            .is_none()
        {
            return Err(format!(
                "{name} has no XInput extension; global hotkeys are unavailable"
            ));
        }
        let ver = conn
            .xinput_xi_query_version(2, 2)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if (ver.major_version, ver.minor_version) < (2, 1) {
            return Err(format!(
                "XInput {}.{} on {name}; Pulse needs XInput 2.1+ for raw hotkey events",
                ver.major_version, ver.minor_version
            ));
        }
        conn.xinput_xi_select_events(
            root,
            &[
                xinput::EventMask {
                    deviceid: xinput::Device::ALL_MASTER.into(),
                    mask: vec![xinput::XIEventMask::RAW_KEY_PRESS | xinput::XIEventMask::RAW_KEY_RELEASE],
                },
                xinput::EventMask {
                    deviceid: xinput::Device::ALL.into(),
                    mask: vec![xinput::XIEventMask::HIERARCHY],
                },
            ],
        )
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| format!("cannot select XInput events: {e}"))?;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| e.to_string())?
        .check()
        .map_err(|e| format!("cannot watch root window properties: {e}"))?;

        let atoms = Atoms {
            net_active_window: intern(&conn, "_NET_ACTIVE_WINDOW")?,
            net_wm_name: intern(&conn, "_NET_WM_NAME")?,
            net_wm_pid: intern(&conn, "_NET_WM_PID")?,
            net_client_list: intern(&conn, "_NET_CLIENT_LIST")?,
            utf8_string: intern(&conn, "UTF8_STRING")?,
        };
        let keymap = X11Keymap::detect(&conn).map_err(|e| e.to_string())?;
        let xwayland = pulse_input::xtest::is_xwayland(&conn);
        let xtest = conn
            .extension_information(x11rb::protocol::xtest::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some();
        let session = SessionSummary {
            display: Some(name.clone()),
            xdg_session_type: std::env::var("XDG_SESSION_TYPE").ok(),
            native_x11: !xwayland,
            xwayland,
            xtest,
            xinput2: true,
            vendor: Some(String::from_utf8_lossy(&conn.setup().vendor).into_owned()),
        };
        let mut w = X11Watch {
            conn,
            root,
            display: name,
            keymap,
            synthetic: Vec::new(),
            device_names: Vec::new(),
            atoms,
            watched: None,
            ewmh: false,
            xtest_is_physical,
            session,
        };
        w.refresh_devices()?;
        Ok(w)
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.conn.stream().as_fd()
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    pub fn keymap_description(&self) -> String {
        format!("{:?}", self.keymap)
    }

    pub fn synthetic_devices(&self) -> Vec<String> {
        self.device_names.clone()
    }

    /// Whether the X server has registered a device with this name (used to
    /// confirm uinput devices are routed before reporting Ready).
    pub fn has_device(&self, name: &str) -> bool {
        self.device_names.iter().any(|n| n == name)
    }

    pub fn has_pulse_uinput_devices(&mut self) -> bool {
        let _ = self.refresh_devices();
        self.has_device(KEYBOARD_NAME) && self.has_device(MOUSE_NAME)
    }

    fn refresh_devices(&mut self) -> Result<(), String> {
        let reply = self
            .conn
            .xinput_xi_query_device(xinput::Device::ALL)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        self.synthetic.clear();
        self.device_names.clear();
        for info in reply.infos {
            let name = String::from_utf8_lossy(&info.name).into_owned();
            let is_slave = matches!(
                info.type_,
                xinput::DeviceType::SLAVE_KEYBOARD
                    | xinput::DeviceType::SLAVE_POINTER
                    | xinput::DeviceType::FLOATING_SLAVE
            );
            let xtest = name.contains("XTEST") && !self.xtest_is_physical;
            if is_slave && (xtest || name == KEYBOARD_NAME || name == MOUSE_NAME) {
                self.synthetic.push(info.deviceid);
                self.device_names.push(name);
            }
        }
        Ok(())
    }

    fn get_prop(&self, win: u32, prop: u32, ty: u32, len: u32) -> Option<Cookie<'_, RustConnection, GetPropertyReply>> {
        self.conn.get_property(false, win, prop, ty, 0, len).ok()
    }

    /// Window metadata with pipelined requests (one round trip).
    pub fn window_info(&self, win: u32) -> Option<WindowInfo> {
        let class = self.get_prop(win, AtomEnum::WM_CLASS.into(), AtomEnum::STRING.into(), 256);
        let net_name = self.get_prop(win, self.atoms.net_wm_name, self.atoms.utf8_string, 256);
        let name = self.get_prop(win, AtomEnum::WM_NAME.into(), AtomEnum::ANY.into(), 256);
        let pid = self.get_prop(win, self.atoms.net_wm_pid, AtomEnum::CARDINAL.into(), 1);
        let class = class?.reply().ok()?;
        let mut parts = class
            .value
            .split(|b| *b == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned());
        let instance = parts.next().unwrap_or_default();
        let class_name = parts.next().unwrap_or_default();
        let net_name = net_name
            .and_then(|c| c.reply().ok())
            .map(|r| prop_text(&r))
            .unwrap_or_default();
        let wm_name = name
            .and_then(|c| c.reply().ok())
            .map(|r| prop_text(&r))
            .unwrap_or_default();
        let pid = pid
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()));
        Some(WindowInfo {
            id: win,
            instance,
            class: class_name,
            title: if net_name.is_empty() { wm_name } else { net_name },
            pid,
        })
    }

    fn active_window_id(&mut self) -> Option<u32> {
        let r = self
            .get_prop(self.root, self.atoms.net_active_window, AtomEnum::WINDOW.into(), 1)?
            .reply()
            .ok()?;
        self.ewmh = r.type_ != x11rb::NONE;
        r.value32().and_then(|mut v| v.next()).filter(|w| *w != 0)
    }

    /// Current active window; also (re)subscribes to its title changes.
    pub fn active_window(&mut self) -> Option<WindowInfo> {
        let id = self.active_window_id();
        if id != self.watched {
            if let Some(old) = self.watched {
                let _ = self
                    .conn
                    .change_window_attributes(old, &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT));
            }
            if let Some(new) = id {
                let _ = self.conn.change_window_attributes(
                    new,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
                );
            }
            self.watched = id;
            let _ = self.conn.flush();
        }
        id.and_then(|w| self.window_info(w))
    }

    /// Whether the window manager publishes `_NET_ACTIVE_WINDOW`.
    pub fn has_ewmh_active_window(&self) -> bool {
        self.ewmh
    }

    pub fn list_windows(&self) -> Vec<WindowInfo> {
        let Some(reply) = self
            .get_prop(self.root, self.atoms.net_client_list, AtomEnum::WINDOW.into(), 1024)
            .and_then(|c| c.reply().ok())
        else {
            return Vec::new();
        };
        let ids: Vec<u32> = reply.value32().map(|v| v.collect()).unwrap_or_default();
        ids.into_iter().filter_map(|w| self.window_info(w)).collect()
    }

    /// Drain all buffered and readable X events without blocking.
    pub fn poll_events(&mut self, out: &mut Vec<X11Event>) -> Result<(), String> {
        loop {
            let ev = match self.conn.poll_for_event() {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(e) => return Err(format!("X11 connection lost: {e}")),
            };
            match ev {
                Event::XinputRawKeyPress(e) => self.raw_key(e.sourceid, e.detail, true, out),
                Event::XinputRawKeyRelease(e) => self.raw_key(e.sourceid, e.detail, false, out),
                Event::XinputHierarchy(_) => {
                    let _ = self.refresh_devices();
                    out.push(X11Event::DevicesChanged);
                }
                Event::PropertyNotify(e) => {
                    let title_change = Some(e.window) == self.watched
                        && (e.atom == self.atoms.net_wm_name
                            || e.atom == u32::from(AtomEnum::WM_NAME)
                            || e.atom == u32::from(AtomEnum::WM_CLASS));
                    if (e.window == self.root && e.atom == self.atoms.net_active_window) || title_change {
                        let w = self.active_window();
                        out.push(X11Event::FocusChanged(w));
                    }
                }
                Event::MappingNotify(e) if e.request == xproto::Mapping::KEYBOARD => {
                    if let Ok(k) = X11Keymap::detect(&self.conn) {
                        self.keymap = k;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn raw_key(&self, source: u16, detail: u32, pressed: bool, out: &mut Vec<X11Event>) {
        if self.synthetic.contains(&source) {
            return;
        }
        let Ok(kc) = u8::try_from(detail) else { return };
        if let Some(key) = self.keymap.key_for_x11(kc) {
            out.push(X11Event::Key {
                key,
                pressed,
                received_at: monotonic_now(),
            });
        }
    }
}
