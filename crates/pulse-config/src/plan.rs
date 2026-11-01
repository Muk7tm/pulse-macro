//! Turn the configuration into one validated, runnable clicker plan.
//!
//! The target-window requirement is not checked here: a missing or unfocused
//! game window is a reason not to *start*, not a reason to reject settings.

use pulse_core::clicker::{ClickerSpec, compile};
use pulse_core::{ActivationMode, Hotkey, Issue, Limits, Pressable, Program, TargetRule, ValidationFailed};

use crate::Config;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClickPlan {
    pub program: Program,
    pub warnings: Vec<Issue>,
    pub activation_hotkey: Hotkey,
    pub activation_mode: ActivationMode,
    pub emergency_stop_hotkey: Hotkey,
    pub require_focus: bool,
    pub target: TargetRule,
}

/// Validate the clicker settings and hotkeys into a plan.
pub fn plan(cfg: &Config) -> Result<ClickPlan, ValidationFailed> {
    let c = &cfg.clicker;
    let mut issues: Vec<Issue> = cfg.validate();
    let compiled = compile(
        &ClickerSpec {
            target: c.target(),
            interval_ms: c.interval_ms,
            hold_ms: c.hold_ms,
            count: c.count,
            duration_ms: c.duration_ms,
        },
        &Limits::for_advanced(cfg.engine.allow_advanced_rates),
    );
    let program = match compiled {
        Ok(cp) => {
            issues.extend(cp.warnings);
            Some(cp.program)
        }
        Err(e) => {
            issues.extend(e.issues);
            None
        }
    };
    if c.activation_hotkey
        .key()
        .is_some_and(|k| c.target() == Pressable::Key(k))
    {
        issues.push(Issue::warning(
            "clicker.key",
            format!(
                "the clicker repeats its own hotkey {}; Pulse ignores its own key presses, but the game will see them",
                c.activation_hotkey
            ),
        ));
    }
    let warnings = pulse_core::validation::finish(issues)?;
    let program = program.ok_or_else(|| ValidationFailed::single("clicker", "invalid clicker settings"))?;
    Ok(ClickPlan {
        program,
        warnings,
        activation_hotkey: c.activation_hotkey,
        activation_mode: c.activation_mode,
        emergency_stop_hotkey: c.emergency_stop_hotkey,
        require_focus: cfg.target.require_focus,
        target: cfg.target.rule(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_core::{Key, MouseButton};

    #[test]
    fn default_plan() {
        let p = plan(&Config::default()).expect("valid");
        assert_eq!(p.program.target, Pressable::Button(MouseButton::Left));
        assert_eq!(p.activation_hotkey, Hotkey::Key(Key::E));
        assert_eq!(p.activation_mode, ActivationMode::Hold);
        assert_eq!(p.emergency_stop_hotkey, Hotkey::Key(Key::F8));
        assert!(!p.require_focus, "clicking anywhere is the default");
        // The default 10 ms speed only carries the Sober chat caution.
        assert!(!p.warnings.is_empty());
        assert!(p.warnings.iter().all(|w| w.path == "clicker.interval_ms"));
    }

    #[test]
    fn keyboard_clicker_plan_and_self_hotkey_warning() {
        let mut cfg = Config::default();
        cfg.clicker.set_target(Pressable::Key(Key::E));
        cfg.clicker.activation_hotkey = Hotkey::Key(Key::E);
        let p = plan(&cfg).expect("valid with a warning");
        assert!(p.warnings.iter().any(|w| w.message.contains("its own hotkey")));
    }

    #[test]
    fn timing_errors_have_field_paths() {
        let mut cfg = Config::default();
        cfg.clicker.hold_ms = 150;
        let err = plan(&cfg).expect_err("hold >= interval");
        assert!(err.issues.iter().any(|i| i.path == "clicker.hold_ms"));
    }

    #[test]
    fn advanced_rates_flag_controls_minimum() {
        let mut cfg = Config::default();
        cfg.clicker.interval_ms = 5;
        cfg.clicker.hold_ms = 2;
        assert!(plan(&cfg).is_err());
        cfg.engine.allow_advanced_rates = true;
        let p = plan(&cfg).expect("ok");
        assert!(p.warnings.iter().any(|w| w.message.contains("experimental")));
    }
}
