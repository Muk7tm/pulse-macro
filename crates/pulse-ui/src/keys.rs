//! Map Slint key-event text to Pulse key names.
//!
//! Slint reports named keys as private-use characters (`slint::platform::Key`)
//! and printable keys as the produced text. Pulse binds *physical* keys, so
//! printable characters are mapped to their US-layout position; on other
//! layouts letters may land on a different physical key — the key dropdown
//! is the exact fallback. Function keys are layout-independent.

use pulse_core::Key;
use slint::platform::Key as SKey;

const NAMED: &[(SKey, &str)] = &[
    (SKey::Backspace, "Backspace"),
    (SKey::Tab, "Tab"),
    (SKey::Return, "Enter"),
    (SKey::Escape, "Escape"),
    (SKey::Delete, "Delete"),
    (SKey::Shift, "LeftShift"),
    (SKey::ShiftR, "RightShift"),
    (SKey::Control, "LeftCtrl"),
    (SKey::ControlR, "RightCtrl"),
    (SKey::Alt, "LeftAlt"),
    (SKey::AltGr, "RightAlt"),
    (SKey::CapsLock, "CapsLock"),
    (SKey::Meta, "LeftMeta"),
    (SKey::MetaR, "RightMeta"),
    (SKey::Space, "Space"),
    (SKey::UpArrow, "Up"),
    (SKey::DownArrow, "Down"),
    (SKey::LeftArrow, "Left"),
    (SKey::RightArrow, "Right"),
    (SKey::Insert, "Insert"),
    (SKey::Home, "Home"),
    (SKey::End, "End"),
    (SKey::PageUp, "PageUp"),
    (SKey::PageDown, "PageDown"),
    (SKey::ScrollLock, "ScrollLock"),
    (SKey::Pause, "Pause"),
    (SKey::SysReq, "PrintScreen"),
    (SKey::Menu, "Menu"),
    (SKey::F1, "F1"),
    (SKey::F2, "F2"),
    (SKey::F3, "F3"),
    (SKey::F4, "F4"),
    (SKey::F5, "F5"),
    (SKey::F6, "F6"),
    (SKey::F7, "F7"),
    (SKey::F8, "F8"),
    (SKey::F9, "F9"),
    (SKey::F10, "F10"),
    (SKey::F11, "F11"),
    (SKey::F12, "F12"),
    (SKey::F13, "F13"),
    (SKey::F14, "F14"),
    (SKey::F15, "F15"),
    (SKey::F16, "F16"),
    (SKey::F17, "F17"),
    (SKey::F18, "F18"),
    (SKey::F19, "F19"),
    (SKey::F20, "F20"),
    (SKey::F21, "F21"),
    (SKey::F22, "F22"),
    (SKey::F23, "F23"),
    (SKey::F24, "F24"),
];

fn printable(c: char) -> Option<String> {
    let name = match c {
        'a'..='z' | 'A'..='Z' | '0'..='9' => return Some(c.to_ascii_uppercase().to_string()),
        '-' | '_' => "Minus",
        '=' | '+' => "Equal",
        '[' | '{' => "LeftBracket",
        ']' | '}' => "RightBracket",
        ';' | ':' => "Semicolon",
        '\'' | '"' => "Apostrophe",
        '`' | '~' => "Grave",
        '\\' | '|' => "Backslash",
        ',' | '<' => "Comma",
        '.' | '>' => "Period",
        '/' | '?' => "Slash",
        '!' => "1",
        '@' => "2",
        '#' => "3",
        '$' => "4",
        '%' => "5",
        '^' => "6",
        '&' => "7",
        '*' => "8",
        '(' => "9",
        ')' => "0",
        _ => return None,
    };
    Some(name.to_owned())
}

/// Canonical Pulse key name for a Slint key event text, if supported.
pub fn key_name_from_slint(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    let name = NAMED
        .iter()
        .find(|(k, _)| char::from(*k) == c)
        .map(|(_, n)| (*n).to_owned())
        .or_else(|| printable(c))?;
    // Only names Pulse actually knows.
    Key::parse(&name).ok().map(|k| k.name().to_owned())
}

/// All bindable key names, for dropdowns.
pub fn all_key_names() -> Vec<slint::SharedString> {
    Key::all().map(|k| k.name().into()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_named_and_printable_keys() {
        assert_eq!(
            key_name_from_slint(&char::from(SKey::F6).to_string()).as_deref(),
            Some("F6")
        );
        assert_eq!(
            key_name_from_slint(&char::from(SKey::Shift).to_string()).as_deref(),
            Some("LeftShift")
        );
        assert_eq!(key_name_from_slint("w").as_deref(), Some("W"));
        assert_eq!(key_name_from_slint("?").as_deref(), Some("Slash"));
        assert_eq!(key_name_from_slint(" ").as_deref(), Some("Space"));
        assert_eq!(key_name_from_slint("é"), None);
        assert_eq!(key_name_from_slint("ab"), None);
        assert_eq!(key_name_from_slint(""), None);
    }
}
