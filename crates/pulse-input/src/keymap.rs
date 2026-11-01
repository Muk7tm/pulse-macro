//! Translation between Pulse's physical keys (evdev codes) and X11 keycodes.
//!
//! Modern Xorg servers (libinput/evdev drivers, `evdev` XKB rules) use
//! `x11_keycode = evdev_code + 8`. That is verified against the live keymap
//! rather than assumed: if layout-independent keys (Escape, Tab, Return,
//! Space, F1) are not where the evdev scheme puts them, Pulse falls back to a
//! keysym lookup using US level-0 keysyms.

use pulse_core::Key;
use pulse_core::input::{KEY_CODE_MAX, KEY_TABLE};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;

use crate::InputError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeymapMode {
    /// `x11 = evdev + 8`, verified.
    Evdev,
    /// Mapped through keysyms (non-evdev X server).
    Keysym,
}

#[derive(Clone)]
pub struct X11Keymap {
    to_x11: [u8; KEY_CODE_MAX as usize + 1],
    from_x11: [u16; 256],
    mode: KeymapMode,
    min_keycode: u8,
    max_keycode: u8,
}

impl std::fmt::Debug for X11Keymap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Keymap")
            .field("mode", &self.mode)
            .field("min_keycode", &self.min_keycode)
            .field("max_keycode", &self.max_keycode)
            .field("mapped", &self.mapped_count())
            .finish()
    }
}

/// Layout-independent reference keys: (evdev code, keysym).
const PROBES: [(u16, u32); 5] = [(1, 0xff1b), (15, 0xff09), (28, 0xff0d), (57, 0x20), (59, 0xffbe)];

impl X11Keymap {
    /// Build from a keyboard mapping: `keysyms[(kc - min) * per + level]`.
    pub fn from_mapping(min_keycode: u8, max_keycode: u8, per_keycode: u8, keysyms: &[u32]) -> X11Keymap {
        let per = per_keycode.max(1) as usize;
        let level0 = |kc: u16| -> Option<u32> {
            if kc < min_keycode as u16 || kc > max_keycode as u16 {
                return None;
            }
            keysyms.get((kc - min_keycode as u16) as usize * per).copied()
        };
        let evdev_hits = PROBES
            .iter()
            .filter(|(code, sym)| level0(code + 8) == Some(*sym))
            .count();
        let mode = if evdev_hits >= 4 {
            KeymapMode::Evdev
        } else {
            KeymapMode::Keysym
        };
        let mut map = X11Keymap {
            to_x11: [0; KEY_CODE_MAX as usize + 1],
            from_x11: [0; 256],
            mode,
            min_keycode,
            max_keycode,
        };
        for info in KEY_TABLE {
            let kc = match mode {
                KeymapMode::Evdev => {
                    let kc = info.code + 8;
                    (kc >= min_keycode as u16 && kc <= max_keycode as u16).then_some(kc)
                }
                KeymapMode::Keysym => (min_keycode as u16..=max_keycode as u16).find(|kc| {
                    let base = (*kc - min_keycode as u16) as usize * per;
                    keysyms
                        .get(base..base + per)
                        .is_some_and(|syms| syms.contains(&info.us_keysym))
                }),
            };
            if let Some(kc) = kc.and_then(|k| u8::try_from(k).ok()) {
                map.to_x11[info.code as usize] = kc;
                if map.from_x11[kc as usize] == 0 {
                    map.from_x11[kc as usize] = info.code;
                }
            }
        }
        map
    }

    pub fn detect<C: Connection>(conn: &C) -> Result<X11Keymap, InputError> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let count = max.saturating_sub(min).saturating_add(1);
        let reply = conn
            .get_keyboard_mapping(min, count)
            .map_err(|e| InputError::Disconnected(e.to_string()))?
            .reply()
            .map_err(|e| InputError::Disconnected(e.to_string()))?;
        Ok(X11Keymap::from_mapping(
            min,
            max,
            reply.keysyms_per_keycode,
            &reply.keysyms,
        ))
    }

    pub fn mode(&self) -> KeymapMode {
        self.mode
    }

    pub fn x11_keycode(&self, key: Key) -> Option<u8> {
        match self.to_x11[key.code() as usize] {
            0 => None,
            kc => Some(kc),
        }
    }

    pub fn key_for_x11(&self, keycode: u8) -> Option<Key> {
        match self.from_x11[keycode as usize] {
            0 => None,
            code => Key::from_code(code),
        }
    }

    pub fn mapped_count(&self) -> usize {
        self.to_x11.iter().filter(|k| **k != 0).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny synthetic evdev-style keymap: keycodes 8..=255, 2 levels.
    fn evdev_mapping() -> (u8, u8, u8, Vec<u32>) {
        let mut syms = vec![0u32; 248 * 2];
        for info in KEY_TABLE {
            let kc = info.code as usize + 8;
            if kc <= 255 {
                syms[(kc - 8) * 2] = info.us_keysym;
            }
        }
        (8, 255, 2, syms)
    }

    #[test]
    fn detects_evdev_layout() {
        let (min, max, per, syms) = evdev_mapping();
        let m = X11Keymap::from_mapping(min, max, per, &syms);
        assert_eq!(m.mode(), KeymapMode::Evdev);
        assert_eq!(m.x11_keycode(Key::F6), Some(72));
        let a = Key::parse("A").expect("key");
        assert_eq!(m.x11_keycode(a), Some(38));
        assert_eq!(m.key_for_x11(38), Some(a));
        assert_eq!(m.key_for_x11(0), None);
    }

    #[test]
    fn evdev_mode_is_layout_independent() {
        // An AZERTY-like layout swaps letters but keeps Escape/Tab/Return/Space/F1.
        let (min, max, per, mut syms) = evdev_mapping();
        syms[(38 - 8) * 2] = 0x71; // 'q' on the physical A key
        let m = X11Keymap::from_mapping(min, max, per, &syms);
        assert_eq!(m.mode(), KeymapMode::Evdev);
        assert_eq!(
            m.x11_keycode(Key::parse("A").expect("key")),
            Some(38),
            "physical position kept"
        );
    }

    #[test]
    fn falls_back_to_keysyms() {
        // Non-evdev server: keycodes shifted by +100 from evdev.
        let mut syms = vec![0u32; 256];
        for info in KEY_TABLE.iter().filter(|i| i.code < 100) {
            syms[(info.code + 100 - 8) as usize] = info.us_keysym;
        }
        let m = X11Keymap::from_mapping(8, 255, 1, &syms);
        assert_eq!(m.mode(), KeymapMode::Keysym);
        assert_eq!(m.x11_keycode(Key::F6), Some(164));
        assert_eq!(m.key_for_x11(164), Some(Key::F6));
    }
}
