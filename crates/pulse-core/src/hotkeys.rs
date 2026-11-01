//! Hotkeys and the pure trigger logic.
//!
//! A hotkey is a keyboard key or a mouse side button. The engine feeds
//! *physical* transitions (synthetic events from Pulse's own backends are
//! filtered out before this point). The controller debounces auto-repeat,
//! maps the activation mode to start/stop actions and handles the dedicated
//! emergency-stop hotkey. The hotkey is live whenever the engine runs.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::input::{Key, UnknownName};
use crate::state::ActivationMode;

/// A mouse side (thumb) button. Pulse only clicks left, right and middle, so
/// a side-button hotkey can never be triggered by Pulse's own clicks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SideButton {
    /// The back button: X11 button 8, usually called Mouse4.
    Back,
    /// The forward button: X11 button 9, usually called Mouse5.
    Forward,
}

impl SideButton {
    pub const ALL: [SideButton; 2] = [SideButton::Back, SideButton::Forward];

    pub const fn name(self) -> &'static str {
        match self {
            SideButton::Back => "Mouse4",
            SideButton::Forward => "Mouse5",
        }
    }

    pub const fn x11_button(self) -> u8 {
        match self {
            SideButton::Back => 8,
            SideButton::Forward => 9,
        }
    }

    pub const fn from_x11_button(button: u32) -> Option<SideButton> {
        match button {
            8 => Some(SideButton::Back),
            9 => Some(SideButton::Forward),
            _ => None,
        }
    }

    fn parse(s: &str) -> Option<SideButton> {
        match s.trim().to_ascii_lowercase().as_str() {
            "mouse4" | "mouseback" | "xbutton1" => Some(SideButton::Back),
            "mouse5" | "mouseforward" | "xbutton2" => Some(SideButton::Forward),
            _ => None,
        }
    }
}

/// What starts/stops the clicker or stops everything. Stored by name, e.g.
/// "F6" or "Mouse4".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Hotkey {
    Key(Key),
    Mouse(SideButton),
}

impl Hotkey {
    pub fn name(self) -> &'static str {
        match self {
            Hotkey::Key(k) => k.name(),
            Hotkey::Mouse(b) => b.name(),
        }
    }

    pub fn parse(s: &str) -> Result<Hotkey, UnknownName> {
        match SideButton::parse(s) {
            Some(b) => Ok(Hotkey::Mouse(b)),
            None => Key::parse(s).map(Hotkey::Key),
        }
    }

    /// The keyboard key, if this hotkey is one.
    pub const fn key(self) -> Option<Key> {
        match self {
            Hotkey::Key(k) => Some(k),
            Hotkey::Mouse(_) => None,
        }
    }

    pub const fn is_mouse(self) -> bool {
        matches!(self, Hotkey::Mouse(_))
    }

    /// Every bindable hotkey: the side buttons first, then all keys.
    pub fn all() -> impl Iterator<Item = Hotkey> {
        SideButton::ALL
            .into_iter()
            .map(Hotkey::Mouse)
            .chain(Key::all().map(Hotkey::Key))
    }
}

impl From<Key> for Hotkey {
    fn from(k: Key) -> Hotkey {
        Hotkey::Key(k)
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Hotkey {
    type Err = UnknownName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Hotkey::parse(s)
    }
}

impl Serialize for Hotkey {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}

impl<'de> Deserialize<'de> for Hotkey {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hotkey::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggerConfig {
    pub activation: Hotkey,
    pub mode: ActivationMode,
    pub emergency_stop: Hotkey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerAction {
    None,
    Start,
    Stop,
    /// Hold mode: activation key released while running.
    Released,
    EmergencyStop,
}

#[derive(Clone, Debug)]
pub struct TriggerController {
    cfg: TriggerConfig,
    activation_down: bool,
    emergency_down: bool,
}

impl TriggerController {
    pub fn new(cfg: TriggerConfig) -> Self {
        TriggerController {
            cfg,
            activation_down: false,
            emergency_down: false,
        }
    }

    pub fn config(&self) -> &TriggerConfig {
        &self.cfg
    }

    /// Replace the bindings; pressed-state tracking restarts.
    pub fn reconfigure(&mut self, cfg: TriggerConfig) {
        self.cfg = cfg;
        self.reset();
    }

    /// Forget pressed state (X11 reconnection or lost releases).
    pub fn reset(&mut self) {
        self.activation_down = false;
        self.emergency_down = false;
    }

    /// Process one physical key or side-button transition. `running` is
    /// whether a run is currently active.
    pub fn on_input(&mut self, input: Hotkey, pressed: bool, running: bool) -> TriggerAction {
        if input == self.cfg.emergency_stop {
            let was = self.emergency_down;
            self.emergency_down = pressed;
            return if pressed && !was {
                TriggerAction::EmergencyStop
            } else {
                TriggerAction::None
            };
        }
        if input != self.cfg.activation {
            return TriggerAction::None;
        }
        let was = self.activation_down;
        self.activation_down = pressed;
        match (pressed, was, self.cfg.mode) {
            // Auto-repeat or duplicate press: ignore until a release.
            (true, true, _) => TriggerAction::None,
            // Release without a recorded press.
            (false, false, _) => TriggerAction::None,
            (true, false, ActivationMode::Toggle) if running => TriggerAction::Stop,
            (true, false, ActivationMode::Toggle) => TriggerAction::Start,
            (true, false, ActivationMode::Hold) if running => TriggerAction::None,
            (true, false, ActivationMode::Hold) => TriggerAction::Start,
            (false, true, ActivationMode::Toggle) => TriggerAction::None,
            (false, true, ActivationMode::Hold) if running => TriggerAction::Released,
            (false, true, ActivationMode::Hold) => TriggerAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F6: Hotkey = Hotkey::Key(Key::F6);
    const F8: Hotkey = Hotkey::Key(Key::F8);

    fn key(n: &str) -> Hotkey {
        Hotkey::parse(n).expect("key")
    }

    fn ctl(mode: ActivationMode) -> TriggerController {
        TriggerController::new(TriggerConfig {
            activation: F6,
            mode,
            emergency_stop: F8,
        })
    }

    #[test]
    fn toggle_ignores_autorepeat_and_release() {
        let mut c = ctl(ActivationMode::Toggle);
        assert_eq!(c.on_input(F6, true, false), TriggerAction::Start);
        assert_eq!(c.on_input(F6, true, true), TriggerAction::None);
        assert_eq!(c.on_input(F6, true, true), TriggerAction::None);
        assert_eq!(c.on_input(F6, false, true), TriggerAction::None);
        assert_eq!(c.on_input(F6, true, true), TriggerAction::Stop);
        assert_eq!(c.on_input(F6, false, false), TriggerAction::None);
    }

    #[test]
    fn hold_starts_and_releases() {
        let mut c = ctl(ActivationMode::Hold);
        assert_eq!(c.on_input(F6, true, false), TriggerAction::Start);
        assert_eq!(c.on_input(F6, true, true), TriggerAction::None);
        assert_eq!(c.on_input(F6, false, true), TriggerAction::Released);
        // Release after the run already ended is harmless.
        assert_eq!(c.on_input(F6, true, false), TriggerAction::Start);
        assert_eq!(c.on_input(F6, false, false), TriggerAction::None);
    }

    #[test]
    fn emergency_is_edge_triggered_and_independent() {
        let mut c = ctl(ActivationMode::Toggle);
        assert_eq!(c.on_input(F8, true, true), TriggerAction::EmergencyStop);
        assert_eq!(c.on_input(F8, true, true), TriggerAction::None);
        assert_eq!(c.on_input(F8, false, true), TriggerAction::None);
        assert_eq!(c.on_input(F8, true, false), TriggerAction::EmergencyStop);
    }

    #[test]
    fn unrelated_keys_are_ignored() {
        let mut c = ctl(ActivationMode::Toggle);
        assert_eq!(c.on_input(key("A"), true, false), TriggerAction::None);
        assert_eq!(c.on_input(key("A"), false, false), TriggerAction::None);
    }

    #[test]
    fn side_buttons_work_like_keys() {
        let back = Hotkey::Mouse(SideButton::Back);
        let mut c = TriggerController::new(TriggerConfig {
            activation: back,
            mode: ActivationMode::Hold,
            emergency_stop: Hotkey::Mouse(SideButton::Forward),
        });
        assert_eq!(c.on_input(back, true, false), TriggerAction::Start);
        assert_eq!(c.on_input(back, false, true), TriggerAction::Released);
        assert_eq!(c.on_input(F6, true, false), TriggerAction::None);
        assert_eq!(
            c.on_input(Hotkey::Mouse(SideButton::Forward), true, true),
            TriggerAction::EmergencyStop
        );
    }

    #[test]
    fn hotkey_names_parse_and_serialize() {
        assert_eq!(key("mouse4"), Hotkey::Mouse(SideButton::Back));
        assert_eq!(key("XButton2"), Hotkey::Mouse(SideButton::Forward));
        assert_eq!(key("f6"), F6);
        assert_eq!(key("Mouse5").to_string(), "Mouse5");
        assert!(Hotkey::parse("Mouse9").is_err());
        assert_eq!(SideButton::from_x11_button(8), Some(SideButton::Back));
        assert_eq!(SideButton::from_x11_button(1), None, "left click is never a hotkey");
        let all: Vec<Hotkey> = Hotkey::all().collect();
        assert_eq!(
            &all[..2],
            &[Hotkey::Mouse(SideButton::Back), Hotkey::Mouse(SideButton::Forward)]
        );
        assert!(all.contains(&F6));
        let json = serde_json::to_string(&Hotkey::Mouse(SideButton::Back)).expect("serialize");
        assert_eq!(json, "\"Mouse4\"");
        assert_eq!(serde_json::from_str::<Hotkey>("\"F8\"").expect("deserialize"), F8);
    }

    #[test]
    fn lost_release_recovery() {
        let mut c = ctl(ActivationMode::Toggle);
        assert_eq!(c.on_input(F6, true, false), TriggerAction::Start);
        // Release lost (e.g. X11 reconnect); without reset the next press is ignored.
        assert_eq!(c.on_input(F6, true, true), TriggerAction::None);
        c.reset();
        assert_eq!(c.on_input(F6, true, true), TriggerAction::Stop);
    }
}
