//! X11 XTest backend.
//!
//! One persistent X11 connection per backend instance. Each logical event is
//! a single XTEST `FakeInput` request written from a stack buffer (x11rb's
//! generated wrapper allocates a `Vec` per request); the connection is
//! flushed once per batch so press and release reach the server at their own
//! deadlines. There is no round trip per event.
//!
//! x11rb records every request it sends and only prunes that record when it
//! reads a packet from the server. `FakeInput` has no reply, so a write-only
//! connection would grow without bound; every `SYNC_INTERVAL` requests a
//! reply-bearing `GetInputFocus` is queued and pending input is drained
//! non-blockingly, which bounds memory and surfaces asynchronous X errors.

use std::io::IoSlice;

use pulse_core::{BackendKind, HeldInputs, KeyState, LogicalInput, MouseButton, Pressable};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::{Event, xtest};
use x11rb::rust_connection::RustConnection;

use crate::backend::{BackendHealth, InputBackend, InputError};
use crate::keymap::{KeymapMode, X11Keymap};

const SYNC_INTERVAL: u32 = 256;

/// XTEST FakeInput request size (protocol: 36 bytes, length field = 9).
pub const FAKE_INPUT_LEN: usize = 36;

/// Encode an XTEST `FakeInput` request (XTEST 2.2 protocol, minor opcode 2)
/// in the connection's native byte order, exactly as x11rb would.
pub fn encode_fake_input(major_opcode: u8, type_: u8, detail: u8, root: u32, x: i16, y: i16) -> [u8; FAKE_INPUT_LEN] {
    let mut b = [0u8; FAKE_INPUT_LEN];
    b[0] = major_opcode;
    b[1] = xtest::FAKE_INPUT_REQUEST;
    b[2..4].copy_from_slice(&((FAKE_INPUT_LEN / 4) as u16).to_ne_bytes());
    b[4] = type_;
    b[5] = detail;
    // b[8..12]: time = CurrentTime (0)
    b[12..16].copy_from_slice(&root.to_ne_bytes());
    b[24..26].copy_from_slice(&x.to_ne_bytes());
    b[26..28].copy_from_slice(&y.to_ne_bytes());
    // b[35]: deviceid = 0 (core devices)
    b
}

const fn x11_button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
    }
}

pub struct XTestBackend {
    conn: RustConnection,
    opcode: u8,
    display: String,
    version: (u8, u16),
    keymap: X11Keymap,
    held: HeldInputs,
    since_sync: u32,
    last_error: Option<String>,
    dead: bool,
}

impl std::fmt::Debug for XTestBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XTestBackend")
            .field("display", &self.display)
            .field("version", &self.version)
            .field("keymap", &self.keymap)
            .finish()
    }
}

/// Resolve a display name for messages.
pub fn display_name(display: Option<&str>) -> String {
    display
        .map(str::to_owned)
        .or_else(|| std::env::var("DISPLAY").ok())
        .unwrap_or_else(|| "(unset DISPLAY)".to_owned())
}

/// Detect an Xwayland server: it advertises the `XWAYLAND` extension.
pub fn is_xwayland<C: RequestConnection>(conn: &C) -> bool {
    conn.extension_information("XWAYLAND").ok().flatten().is_some()
}

impl XTestBackend {
    /// Connect, verify XTEST, and load the keymap. `allow_xwayland` permits
    /// running against Xwayland (not officially supported: only
    /// Xwayland-managed windows would receive input).
    pub fn open(display: Option<&str>, allow_xwayland: bool) -> Result<XTestBackend, InputError> {
        let name = display_name(display);
        let (conn, _screen) = x11rb::connect(display).map_err(|e| InputError::CannotConnectX11 {
            display: name.clone(),
            reason: e.to_string(),
        })?;
        let info = conn
            .extension_information(xtest::X11_EXTENSION_NAME)
            .map_err(|e| InputError::CannotConnectX11 {
                display: name.clone(),
                reason: e.to_string(),
            })?
            .ok_or_else(|| InputError::MissingXTest { display: name.clone() })?;
        if !allow_xwayland && is_xwayland(&conn) {
            return Err(InputError::UnsupportedSession(format!(
                "{name} is Xwayland; XTest would only reach Xwayland windows. Pulse supports native X11 sessions"
            )));
        }
        let ver = xtest::get_version(&conn, 2, 2)
            .map_err(|e| InputError::Disconnected(e.to_string()))?
            .reply()
            .map_err(|e| InputError::Disconnected(e.to_string()))?;
        let keymap = X11Keymap::detect(&conn)?;
        Ok(XTestBackend {
            conn,
            opcode: info.major_opcode,
            display: name,
            version: (ver.major_version, ver.minor_version),
            keymap,
            held: HeldInputs::new(),
            since_sync: 0,
            last_error: None,
            dead: false,
        })
    }

    pub fn keymap(&self) -> &X11Keymap {
        &self.keymap
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    fn fail(&mut self, e: InputError) -> InputError {
        if e.is_fatal() {
            self.dead = true;
        }
        self.last_error = Some(e.to_string());
        e
    }

    fn encode(&self, ev: &LogicalInput) -> Result<[u8; FAKE_INPUT_LEN], InputError> {
        let none = x11rb::NONE;
        Ok(match *ev {
            LogicalInput::MouseButton { button, state } => {
                let t = match state {
                    KeyState::Pressed => xproto::BUTTON_PRESS_EVENT,
                    KeyState::Released => xproto::BUTTON_RELEASE_EVENT,
                };
                encode_fake_input(self.opcode, t, x11_button(button), none, 0, 0)
            }
            LogicalInput::KeyboardKey { key, state } => {
                let kc = self.keymap.x11_keycode(key).ok_or_else(|| {
                    InputError::Unsupported(format!("key {key} has no X11 keycode on {}", self.display))
                })?;
                let t = match state {
                    KeyState::Pressed => xproto::KEY_PRESS_EVENT,
                    KeyState::Released => xproto::KEY_RELEASE_EVENT,
                };
                encode_fake_input(self.opcode, t, kc, none, 0, 0)
            }
        })
    }

    fn send(&mut self, buf: &[u8; FAKE_INPUT_LEN]) -> Result<(), InputError> {
        let cookie = self
            .conn
            .send_request_without_reply(&[IoSlice::new(buf)], Vec::new())
            .map_err(|e| InputError::Disconnected(e.to_string()))?;
        // Errors (none expected for prevalidated input) arrive as events and
        // are drained in `maintain`.
        drop(cookie);
        self.since_sync += 1;
        Ok(())
    }

    /// Bound x11rb's request bookkeeping and collect asynchronous errors.
    fn maintain(&mut self) -> Result<(), InputError> {
        if self.since_sync >= SYNC_INTERVAL {
            self.since_sync = 0;
            match self.conn.get_input_focus() {
                Ok(cookie) => cookie.discard_reply_and_errors(),
                Err(e) => return Err(InputError::Disconnected(e.to_string())),
            }
        }
        self.drain_events()
    }

    /// Wait until the server has processed every request sent so far.
    /// Off the hot path only (cleanup, checks, shutdown).
    fn sync(&mut self) -> Result<(), InputError> {
        self.since_sync = 0;
        self.conn
            .get_input_focus()
            .map_err(|e| InputError::Disconnected(e.to_string()))?
            .reply()
            .map(drop)
            .map_err(|e| InputError::Disconnected(e.to_string()))
    }

    fn drain_events(&mut self) -> Result<(), InputError> {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::Error(err))) => {
                    self.last_error = Some(format!(
                        "X11 error: {:?} (request {})",
                        err.error_kind, err.major_opcode
                    ));
                }
                Ok(Some(_)) => {}
                Ok(None) => return Ok(()),
                Err(e) => return Err(InputError::Disconnected(e.to_string())),
            }
        }
    }
}

impl InputBackend for XTestBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::XTest
    }

    fn emit(&mut self, events: &[LogicalInput]) -> Result<(), InputError> {
        if self.dead {
            return Err(InputError::Disconnected("X11 connection previously failed".into()));
        }
        for ev in events {
            let buf = self.encode(ev).map_err(|e| self.fail(e))?;
            self.send(&buf).map_err(|e| self.fail(e))?;
            self.held.apply_lenient(ev);
        }
        self.conn
            .flush()
            .map_err(|e| InputError::Disconnected(e.to_string()))
            .map_err(|e| self.fail(e))?;
        if self.since_sync >= SYNC_INTERVAL {
            self.maintain().map_err(|e| self.fail(e))?;
        }
        Ok(())
    }

    fn release_all(&mut self, extra: &HeldInputs) -> Result<(), InputError> {
        let all = self.held.union(extra);
        let mut first_err = None;
        for p in all.iter() {
            let ev = LogicalInput::release_of(p);
            match self.encode(&ev).and_then(|b| self.send(&b)) {
                Ok(()) => self.held.apply_lenient(&ev),
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        if let Err(e) = self.conn.flush() {
            first_err.get_or_insert(InputError::Disconnected(e.to_string()));
        }
        // Cleanup path: confirm the server processed the releases, so a
        // following disconnect cannot discard them (see `sync`).
        if first_err.is_none() {
            if let Err(e) = self.sync() {
                first_err = Some(e);
            }
        }
        match first_err {
            Some(e) => Err(self.fail(e)),
            None => Ok(()),
        }
    }

    fn held(&self) -> HeldInputs {
        self.held
    }

    fn check(&mut self) -> Result<(), InputError> {
        if self.dead {
            return Err(InputError::Disconnected("X11 connection previously failed".into()));
        }
        // A real round trip: proves the server is alive and processed
        // everything sent so far. Off the hot path only.
        let r = self
            .conn
            .get_input_focus()
            .map_err(|e| InputError::Disconnected(e.to_string()))
            .and_then(|c| c.reply().map(drop).map_err(|e| InputError::Disconnected(e.to_string())));
        self.since_sync = 0;
        r.and_then(|()| self.drain_events()).map_err(|e| self.fail(e))
    }

    fn supports(&self, set: &HeldInputs) -> Result<(), InputError> {
        for p in set.iter() {
            if let Pressable::Key(k) = p {
                if self.keymap.x11_keycode(k).is_none() {
                    return Err(InputError::Unsupported(format!(
                        "key {k} has no X11 keycode on {}",
                        self.display
                    )));
                }
            }
        }
        Ok(())
    }

    fn health(&self) -> BackendHealth {
        let mode = match self.keymap.mode() {
            KeymapMode::Evdev => "evdev keycodes",
            KeymapMode::Keysym => "keysym-mapped keycodes",
        };
        BackendHealth {
            kind: BackendKind::XTest,
            healthy: !self.dead,
            detail: format!(
                "XTEST {}.{} on {} ({mode}, {} keys mapped)",
                self.version.0,
                self.version.1,
                self.display,
                self.keymap.mapped_count()
            ),
            last_error: self.last_error.clone(),
        }
    }
}

impl Drop for XTestBackend {
    fn drop(&mut self) {
        if self.dead {
            return;
        }
        // Always end with a round trip: when a client disconnects, the X
        // server may discard requests it has not processed yet, which could
        // drop a just-sent release and leave a button stuck down
        // (measured: 2 of 20 without this, 0 of 20 with it).
        if self.held.is_empty() {
            let _ = self.sync();
        } else {
            let _ = self.release_all(&HeldInputs::new());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xtest::FakeInputRequest;

    #[test]
    fn encoding_matches_x11rb_serializer() {
        let cases = [
            (xproto::BUTTON_PRESS_EVENT, 1u8, 0u32, 0i16, 0i16),
            (xproto::KEY_RELEASE_EVENT, 72, 0, 0, 0),
            (xproto::MOTION_NOTIFY_EVENT, 1, 0, -1234, 5678),
            (xproto::BUTTON_RELEASE_EVENT, 3, 0x1234_5678, 7, -7),
        ];
        for (t, detail, root, x, y) in cases {
            let ours = encode_fake_input(140, t, detail, root, x, y);
            let theirs = FakeInputRequest {
                type_: t,
                detail,
                time: 0,
                root,
                root_x: x,
                root_y: y,
                deviceid: 0,
            }
            .serialize(140);
            assert_eq!(&ours[..], &theirs.0[0][..], "type {t}");
        }
    }
}
