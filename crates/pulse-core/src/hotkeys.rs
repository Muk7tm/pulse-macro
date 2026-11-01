//! Pure hotkey trigger logic.
//!
//! The engine feeds *physical* key transitions (synthetic events from Pulse's
//! own backends are filtered out before this point). The controller debounces
//! auto-repeat, maps the activation mode to start/stop actions and handles
//! the dedicated emergency-stop key. The hotkey is live whenever the engine
//! runs.

use crate::input::Key;
use crate::state::ActivationMode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TriggerConfig {
    pub activation: Key,
    pub mode: ActivationMode,
    pub emergency_stop: Key,
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

    /// Process one physical key transition. `running` is whether a run is
    /// currently active.
    pub fn on_key(&mut self, key: Key, pressed: bool, running: bool) -> TriggerAction {
        if key == self.cfg.emergency_stop {
            let was = self.emergency_down;
            self.emergency_down = pressed;
            return if pressed && !was {
                TriggerAction::EmergencyStop
            } else {
                TriggerAction::None
            };
        }
        if key != self.cfg.activation {
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

    fn key(n: &str) -> Key {
        Key::parse(n).expect("key")
    }

    fn ctl(mode: ActivationMode) -> TriggerController {
        TriggerController::new(TriggerConfig {
            activation: Key::F6,
            mode,
            emergency_stop: Key::F8,
        })
    }

    #[test]
    fn toggle_ignores_autorepeat_and_release() {
        let mut c = ctl(ActivationMode::Toggle);
        assert_eq!(c.on_key(Key::F6, true, false), TriggerAction::Start);
        assert_eq!(c.on_key(Key::F6, true, true), TriggerAction::None);
        assert_eq!(c.on_key(Key::F6, true, true), TriggerAction::None);
        assert_eq!(c.on_key(Key::F6, false, true), TriggerAction::None);
        assert_eq!(c.on_key(Key::F6, true, true), TriggerAction::Stop);
        assert_eq!(c.on_key(Key::F6, false, false), TriggerAction::None);
    }

    #[test]
    fn hold_starts_and_releases() {
        let mut c = ctl(ActivationMode::Hold);
        assert_eq!(c.on_key(Key::F6, true, false), TriggerAction::Start);
        assert_eq!(c.on_key(Key::F6, true, true), TriggerAction::None);
        assert_eq!(c.on_key(Key::F6, false, true), TriggerAction::Released);
        // Release after the run already ended is harmless.
        assert_eq!(c.on_key(Key::F6, true, false), TriggerAction::Start);
        assert_eq!(c.on_key(Key::F6, false, false), TriggerAction::None);
    }

    #[test]
    fn emergency_is_edge_triggered_and_independent() {
        let mut c = ctl(ActivationMode::Toggle);
        assert_eq!(c.on_key(Key::F8, true, true), TriggerAction::EmergencyStop);
        assert_eq!(c.on_key(Key::F8, true, true), TriggerAction::None);
        assert_eq!(c.on_key(Key::F8, false, true), TriggerAction::None);
        assert_eq!(c.on_key(Key::F8, true, false), TriggerAction::EmergencyStop);
    }

    #[test]
    fn unrelated_keys_are_ignored() {
        let mut c = ctl(ActivationMode::Toggle);
        assert_eq!(c.on_key(key("A"), true, false), TriggerAction::None);
        assert_eq!(c.on_key(key("A"), false, false), TriggerAction::None);
    }

    #[test]
    fn lost_release_recovery() {
        let mut c = ctl(ActivationMode::Toggle);
        assert_eq!(c.on_key(Key::F6, true, false), TriggerAction::Start);
        // Release lost (e.g. X11 reconnect); without reset the next press is ignored.
        assert_eq!(c.on_key(Key::F6, true, true), TriggerAction::None);
        c.reset();
        assert_eq!(c.on_key(Key::F6, true, true), TriggerAction::Stop);
    }
}
