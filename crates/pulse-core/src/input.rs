//! Normalized, backend-independent input model.
//!
//! Keys are identified by *physical position* using Linux evdev key codes
//! (`KEY_*` from `linux/input-event-codes.h`). Each backend translates them
//! independently: uinput uses the code directly, XTest maps it to an X11
//! keycode after verifying the server keymap. X11 keycodes and evdev codes are
//! never assumed interchangeable outside that translation layer.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Highest evdev key code (`KEY_MAX`).
pub const KEY_CODE_MAX: u16 = 0x2ff;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub const ALL: [MouseButton; 3] = [MouseButton::Left, MouseButton::Right, MouseButton::Middle];

    pub const fn index(self) -> usize {
        match self {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        }
    }
}

impl fmt::Display for MouseButton {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for MouseButton {
    type Err = UnknownName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "left" | "l" | "button1" => Ok(MouseButton::Left),
            "right" | "r" | "button3" => Ok(MouseButton::Right),
            "middle" | "m" | "button2" => Ok(MouseButton::Middle),
            _ => Err(UnknownName(s.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyState {
    Pressed,
    Released,
}

/// A physical keyboard key, stored as its evdev `KEY_*` code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key(u16);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown key or button name `{0}`")]
pub struct UnknownName(pub String);

/// One entry in the static key table.
#[derive(Debug, Clone, Copy)]
pub struct KeyInfo {
    /// Canonical name used in configuration files and the UI.
    pub name: &'static str,
    /// evdev `KEY_*` code.
    pub code: u16,
    /// Level-0 keysym on a US layout. Used only as a fallback when the X
    /// server does not use evdev keycodes.
    pub us_keysym: u32,
}

const fn k(name: &'static str, code: u16, us_keysym: u32) -> KeyInfo {
    KeyInfo { name, code, us_keysym }
}

/// All keys Pulse can emit or bind. Codes from `linux/input-event-codes.h`.
pub static KEY_TABLE: &[KeyInfo] = &[
    k("Escape", 1, 0xff1b),
    k("1", 2, 0x31),
    k("2", 3, 0x32),
    k("3", 4, 0x33),
    k("4", 5, 0x34),
    k("5", 6, 0x35),
    k("6", 7, 0x36),
    k("7", 8, 0x37),
    k("8", 9, 0x38),
    k("9", 10, 0x39),
    k("0", 11, 0x30),
    k("Minus", 12, 0x2d),
    k("Equal", 13, 0x3d),
    k("Backspace", 14, 0xff08),
    k("Tab", 15, 0xff09),
    k("Q", 16, 0x71),
    k("W", 17, 0x77),
    k("E", 18, 0x65),
    k("R", 19, 0x72),
    k("T", 20, 0x74),
    k("Y", 21, 0x79),
    k("U", 22, 0x75),
    k("I", 23, 0x69),
    k("O", 24, 0x6f),
    k("P", 25, 0x70),
    k("LeftBracket", 26, 0x5b),
    k("RightBracket", 27, 0x5d),
    k("Enter", 28, 0xff0d),
    k("LeftCtrl", 29, 0xffe3),
    k("A", 30, 0x61),
    k("S", 31, 0x73),
    k("D", 32, 0x64),
    k("F", 33, 0x66),
    k("G", 34, 0x67),
    k("H", 35, 0x68),
    k("J", 36, 0x6a),
    k("K", 37, 0x6b),
    k("L", 38, 0x6c),
    k("Semicolon", 39, 0x3b),
    k("Apostrophe", 40, 0x27),
    k("Grave", 41, 0x60),
    k("LeftShift", 42, 0xffe1),
    k("Backslash", 43, 0x5c),
    k("Z", 44, 0x7a),
    k("X", 45, 0x78),
    k("C", 46, 0x63),
    k("V", 47, 0x76),
    k("B", 48, 0x62),
    k("N", 49, 0x6e),
    k("M", 50, 0x6d),
    k("Comma", 51, 0x2c),
    k("Period", 52, 0x2e),
    k("Slash", 53, 0x2f),
    k("RightShift", 54, 0xffe2),
    k("KpMultiply", 55, 0xffaa),
    k("LeftAlt", 56, 0xffe9),
    k("Space", 57, 0x20),
    k("CapsLock", 58, 0xffe5),
    k("F1", 59, 0xffbe),
    k("F2", 60, 0xffbf),
    k("F3", 61, 0xffc0),
    k("F4", 62, 0xffc1),
    k("F5", 63, 0xffc2),
    k("F6", 64, 0xffc3),
    k("F7", 65, 0xffc4),
    k("F8", 66, 0xffc5),
    k("F9", 67, 0xffc6),
    k("F10", 68, 0xffc7),
    k("NumLock", 69, 0xff7f),
    k("ScrollLock", 70, 0xff14),
    k("Kp7", 71, 0xff95),
    k("Kp8", 72, 0xff97),
    k("Kp9", 73, 0xff9a),
    k("KpMinus", 74, 0xffad),
    k("Kp4", 75, 0xff96),
    k("Kp5", 76, 0xff9d),
    k("Kp6", 77, 0xff98),
    k("KpPlus", 78, 0xffab),
    k("Kp1", 79, 0xff9c),
    k("Kp2", 80, 0xff99),
    k("Kp3", 81, 0xff9b),
    k("Kp0", 82, 0xff9e),
    k("KpDecimal", 83, 0xff9f),
    k("F11", 87, 0xffc8),
    k("F12", 88, 0xffc9),
    k("KpEnter", 96, 0xff8d),
    k("RightCtrl", 97, 0xffe4),
    k("KpDivide", 98, 0xffaf),
    k("PrintScreen", 99, 0xff61),
    k("RightAlt", 100, 0xffea),
    k("Home", 102, 0xff50),
    k("Up", 103, 0xff52),
    k("PageUp", 104, 0xff55),
    k("Left", 105, 0xff51),
    k("Right", 106, 0xff53),
    k("End", 107, 0xff57),
    k("Down", 108, 0xff54),
    k("PageDown", 109, 0xff56),
    k("Insert", 110, 0xff63),
    k("Delete", 111, 0xffff),
    k("Pause", 119, 0xff13),
    k("LeftMeta", 125, 0xffeb),
    k("RightMeta", 126, 0xffec),
    k("Menu", 127, 0xff67),
    k("F13", 183, 0xffca),
    k("F14", 184, 0xffcb),
    k("F15", 185, 0xffcc),
    k("F16", 186, 0xffcd),
    k("F17", 187, 0xffce),
    k("F18", 188, 0xffcf),
    k("F19", 189, 0xffd0),
    k("F20", 190, 0xffd1),
    k("F21", 191, 0xffd2),
    k("F22", 192, 0xffd3),
    k("F23", 193, 0xffd4),
    k("F24", 194, 0xffd5),
];

/// Accepted alternative spellings, mapped to canonical names.
static KEY_ALIASES: &[(&str, &str)] = &[
    ("esc", "Escape"),
    ("return", "Enter"),
    ("shift", "LeftShift"),
    ("lshift", "LeftShift"),
    ("rshift", "RightShift"),
    ("ctrl", "LeftCtrl"),
    ("control", "LeftCtrl"),
    ("lctrl", "LeftCtrl"),
    ("rctrl", "RightCtrl"),
    ("alt", "LeftAlt"),
    ("lalt", "LeftAlt"),
    ("ralt", "RightAlt"),
    ("altgr", "RightAlt"),
    ("super", "LeftMeta"),
    ("meta", "LeftMeta"),
    ("win", "LeftMeta"),
    ("lmeta", "LeftMeta"),
    ("rmeta", "RightMeta"),
    ("del", "Delete"),
    ("ins", "Insert"),
    ("pgup", "PageUp"),
    ("pgdn", "PageDown"),
    ("pagedn", "PageDown"),
    ("arrowup", "Up"),
    ("arrowdown", "Down"),
    ("arrowleft", "Left"),
    ("arrowright", "Right"),
    ("dash", "Minus"),
    ("equals", "Equal"),
    ("dot", "Period"),
    ("quote", "Apostrophe"),
    ("backtick", "Grave"),
    ("tilde", "Grave"),
    ("bracketleft", "LeftBracket"),
    ("bracketright", "RightBracket"),
    ("print", "PrintScreen"),
    ("sysrq", "PrintScreen"),
    ("caps", "CapsLock"),
];

impl Key {
    /// Default activation hotkey.
    pub const F6: Key = Key(64);
    /// Default emergency-stop hotkey.
    pub const F8: Key = Key(66);
    /// Default key for the keyboard clicker.
    pub const E: Key = Key(18);

    /// Create a key from an evdev code if Pulse knows it.
    pub fn from_code(code: u16) -> Option<Key> {
        KEY_TABLE.iter().any(|e| e.code == code).then_some(Key(code))
    }

    pub const fn code(self) -> u16 {
        self.0
    }

    pub fn info(self) -> &'static KeyInfo {
        // Keys can only be constructed from table entries, so the lookup
        // always succeeds; the fallback keeps this panic-free regardless.
        KEY_TABLE.iter().find(|e| e.code == self.0).unwrap_or(&KEY_TABLE[0])
    }

    pub fn name(self) -> &'static str {
        self.info().name
    }

    pub fn all() -> impl Iterator<Item = Key> {
        KEY_TABLE.iter().map(|e| Key(e.code))
    }

    pub fn parse(s: &str) -> Result<Key, UnknownName> {
        let t = s.trim();
        if let Some(e) = KEY_TABLE.iter().find(|e| e.name.eq_ignore_ascii_case(t)) {
            return Ok(Key(e.code));
        }
        let lower = t.to_ascii_lowercase();
        if let Some((_, canon)) = KEY_ALIASES.iter().find(|(a, _)| *a == lower) {
            if let Some(e) = KEY_TABLE.iter().find(|e| e.name == *canon) {
                return Ok(Key(e.code));
            }
        }
        Err(UnknownName(s.to_owned()))
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Key {
    type Err = UnknownName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Key::parse(s)
    }
}

impl Serialize for Key {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Key::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// One mouse button or one keyboard key: what the clicker repeats, and the
/// unit the held-input ledgers track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pressable {
    Button(MouseButton),
    Key(Key),
}

impl Pressable {
    pub const fn press(self) -> LogicalInput {
        LogicalInput::press_of(self)
    }

    pub const fn release(self) -> LogicalInput {
        LogicalInput::release_of(self)
    }

    /// Short human-readable label, e.g. "Left click" or "E key".
    pub fn label(self) -> String {
        match self {
            Pressable::Button(MouseButton::Left) => "Left click".to_owned(),
            Pressable::Button(MouseButton::Right) => "Right click".to_owned(),
            Pressable::Button(MouseButton::Middle) => "Middle click".to_owned(),
            Pressable::Key(k) => format!("{k} key"),
        }
    }
}

impl fmt::Display for Pressable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pressable::Button(b) => write!(f, "mouse {b}"),
            Pressable::Key(k) => write!(f, "key {k}"),
        }
    }
}

/// A prevalidated logical input event, independent of X11/evdev encodings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalInput {
    MouseButton { button: MouseButton, state: KeyState },
    KeyboardKey { key: Key, state: KeyState },
}

impl LogicalInput {
    pub const fn button(button: MouseButton, state: KeyState) -> Self {
        LogicalInput::MouseButton { button, state }
    }

    pub const fn key(key: Key, state: KeyState) -> Self {
        LogicalInput::KeyboardKey { key, state }
    }

    /// The input this event changes and its new state.
    pub const fn pressable(&self) -> (Pressable, KeyState) {
        match *self {
            LogicalInput::MouseButton { button, state } => (Pressable::Button(button), state),
            LogicalInput::KeyboardKey { key, state } => (Pressable::Key(key), state),
        }
    }

    pub const fn is_press(&self) -> bool {
        matches!(self.pressable().1, KeyState::Pressed)
    }

    pub const fn press_of(p: Pressable) -> Self {
        match p {
            Pressable::Button(button) => LogicalInput::MouseButton {
                button,
                state: KeyState::Pressed,
            },
            Pressable::Key(key) => LogicalInput::KeyboardKey {
                key,
                state: KeyState::Pressed,
            },
        }
    }

    pub const fn release_of(p: Pressable) -> Self {
        match p {
            Pressable::Button(button) => LogicalInput::MouseButton {
                button,
                state: KeyState::Released,
            },
            Pressable::Key(key) => LogicalInput::KeyboardKey {
                key,
                state: KeyState::Released,
            },
        }
    }
}

impl fmt::Display for LogicalInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = |s: &KeyState| match s {
            KeyState::Pressed => "down",
            KeyState::Released => "up",
        };
        match self {
            LogicalInput::MouseButton { button, state } => write!(f, "mouse {button} {}", st(state)),
            LogicalInput::KeyboardKey { key, state } => write!(f, "key {key} {}", st(state)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_table_has_unique_codes_and_names() {
        for (i, a) in KEY_TABLE.iter().enumerate() {
            assert!(a.code <= KEY_CODE_MAX);
            for b in &KEY_TABLE[i + 1..] {
                assert_ne!(a.code, b.code, "{} / {}", a.name, b.name);
                assert!(!a.name.eq_ignore_ascii_case(b.name));
            }
        }
        for (alias, canon) in KEY_ALIASES {
            assert!(Key::parse(canon).is_ok(), "alias {alias} -> {canon}");
        }
    }

    #[test]
    fn parse_is_case_insensitive_and_accepts_aliases() {
        assert_eq!(Key::parse("f6").map(Key::code), Ok(64));
        assert_eq!(Key::parse("ESC").map(Key::code), Ok(1));
        assert_eq!(Key::parse("ctrl").map(Key::code), Ok(29));
        assert_eq!(Key::parse(" a ").map(Key::code), Ok(30));
        assert!(Key::parse("NotAKey").is_err());
        assert!(Key::parse("").is_err());
    }

    #[test]
    fn key_serde_roundtrip_uses_canonical_name() {
        let k = Key::parse("pgup").expect("alias");
        let json = serde_json::to_string(&k).expect("serialize");
        assert_eq!(json, "\"PageUp\"");
        let back: Key = serde_json::from_str("\"pagedn\"").expect("deserialize alias");
        assert_eq!(back.name(), "PageDown");
        assert!(serde_json::from_str::<Key>("\"Nope\"").is_err());
    }

    #[test]
    fn named_constants_match_table() {
        assert_eq!(Key::F6.name(), "F6");
        assert_eq!(Key::F8.name(), "F8");
        assert_eq!(Key::E.name(), "E");
    }

    #[test]
    fn pressable_labels_and_events() {
        let e = Pressable::Key(Key::parse("e").expect("key"));
        assert_eq!(e.label(), "E key");
        assert_eq!(Pressable::Button(MouseButton::Right).label(), "Right click");
        assert!(e.press().is_press());
        assert!(!e.release().is_press());
        assert_eq!(e.release().pressable(), (e, KeyState::Released));
    }

    #[test]
    fn from_code_rejects_unknown() {
        assert!(Key::from_code(0).is_none());
        assert!(Key::from_code(500).is_none());
        assert_eq!(Key::from_code(57).map(Key::name), Some("Space"));
    }
}
