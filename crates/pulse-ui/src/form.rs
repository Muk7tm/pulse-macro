//! Pure mapping between the configuration and the on-screen form, plus the
//! plain-English texts the window shows. No Slint types: unit-testable.

use pulse_config::{Config, InputKind};
use pulse_core::{ActivationMode, Issue, Key, MouseButton, Severity, StopReason, TargetRule, WindowInfo};
use pulse_protocol::StopInfo;

/// Index of "Keyboard key" in the window's "Click" dropdown.
pub const KEYBOARD_INDEX: i32 = 3;

/// The values the form shows. Indices match the Slint controls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormValues {
    pub click_index: i32,
    pub key: String,
    pub interval_ms: i32,
    pub hold_ms: i32,
    /// 0 = toggle, 1 = hold.
    pub mode_index: i32,
    pub hotkey: String,
    pub count: i32,
    pub duration_s: i32,
    pub advanced: bool,
    pub require_focus: bool,
}

fn to_i32(v: u64) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

fn to_u64(v: i32) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Whole seconds shown for a millisecond duration (rounded up, so a limit
/// never displays as "0 = never").
fn duration_display(ms: u64) -> i32 {
    to_i32(ms.div_ceil(1000))
}

pub fn form_from_config(cfg: &Config) -> FormValues {
    let c = &cfg.clicker;
    FormValues {
        click_index: match (c.input, c.mouse_button) {
            (InputKind::Key, _) => KEYBOARD_INDEX,
            (InputKind::Mouse, MouseButton::Left) => 0,
            (InputKind::Mouse, MouseButton::Right) => 1,
            (InputKind::Mouse, MouseButton::Middle) => 2,
        },
        key: c.key.name().to_owned(),
        interval_ms: to_i32(c.interval_ms),
        hold_ms: to_i32(c.hold_ms),
        mode_index: match c.activation_mode {
            ActivationMode::Toggle => 0,
            ActivationMode::Hold => 1,
        },
        hotkey: c.activation_hotkey.name().to_owned(),
        count: to_i32(c.count),
        duration_s: duration_display(c.duration_ms),
        advanced: cfg.engine.allow_advanced_rates,
        require_focus: cfg.target.require_focus,
    }
}

/// A form value that cannot be turned into configuration at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormError {
    pub field: Field,
    pub message: String,
}

/// Build the candidate configuration: `base` (the engine's current config)
/// with the form's values applied. Values the form cannot show exactly
/// (e.g. a duration that is not whole seconds) are kept when unchanged.
pub fn apply_form(base: &Config, f: &FormValues) -> Result<Config, FormError> {
    let mut cfg = base.clone();
    let c = &mut cfg.clicker;
    match f.click_index {
        0 => {
            c.input = InputKind::Mouse;
            c.mouse_button = MouseButton::Left;
        }
        1 => {
            c.input = InputKind::Mouse;
            c.mouse_button = MouseButton::Right;
        }
        2 => {
            c.input = InputKind::Mouse;
            c.mouse_button = MouseButton::Middle;
        }
        _ => c.input = InputKind::Key,
    }
    c.key = Key::parse(&f.key).map_err(|_| FormError {
        field: Field::Key,
        message: format!("Unknown key “{}”", f.key),
    })?;
    c.activation_hotkey = Key::parse(&f.hotkey).map_err(|_| FormError {
        field: Field::Hotkey,
        message: format!("Unknown key “{}”", f.hotkey),
    })?;
    c.interval_ms = to_u64(f.interval_ms);
    c.hold_ms = to_u64(f.hold_ms);
    c.activation_mode = if f.mode_index == 1 {
        ActivationMode::Hold
    } else {
        ActivationMode::Toggle
    };
    c.count = to_u64(f.count);
    if f.duration_s != duration_display(base.clicker.duration_ms) {
        c.duration_ms = to_u64(f.duration_s).saturating_mul(1000);
    }
    cfg.engine.allow_advanced_rates = f.advanced;
    cfg.target.require_focus = f.require_focus;
    Ok(cfg)
}

/// Form areas a validation message can be shown under.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    Click,
    Key,
    Speed,
    Hold,
    Hotkey,
    Count,
    Duration,
    Window,
    General,
}

pub fn field_for(path: &str) -> Field {
    match path {
        "clicker.input" | "clicker.mouse_button" => Field::Click,
        "clicker.key" => Field::Key,
        "clicker.interval_ms" => Field::Speed,
        "clicker.hold_ms" => Field::Hold,
        "clicker.activation_hotkey" | "clicker.emergency_stop_hotkey" | "clicker.activation_mode" => Field::Hotkey,
        "clicker.count" => Field::Count,
        "clicker.duration_ms" => Field::Duration,
        p if p.starts_with("target") => Field::Window,
        _ => Field::General,
    }
}

/// Short, friendly wording for the messages the window shows most often.
fn friendly(issue: &Issue) -> String {
    let m = &issue.message;
    match (issue.path.as_str(), issue.severity) {
        ("clicker.interval_ms", Severity::Warning) => "Under 100 ms can break Roblox chat.".into(),
        ("clicker.interval_ms", Severity::Error) if m.contains("below the minimum") => {
            "Under 10 ms needs “Allow very fast speeds” (More options).".into()
        }
        ("clicker.interval_ms", Severity::Error) if m.contains("greater than 0") => "Must be at least 1 ms.".into(),
        ("clicker.hold_ms", Severity::Error) if m.contains("shorter than the interval") => {
            "Must be shorter than the speed.".into()
        }
        ("clicker.emergency_stop_hotkey", _) => "Can't be the emergency stop key.".into(),
        ("clicker.key", Severity::Error) => "Can't press the emergency stop key.".into(),
        ("clicker.key", Severity::Warning) => "Also your start/stop key; the game sees it.".into(),
        _ => {
            let mut s = m.clone();
            if let Some(first) = s.get(..1) {
                let upper = first.to_uppercase();
                s.replace_range(..1, &upper);
            }
            s
        }
    }
}

/// First error and first warning for each field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Messages {
    pub errors: Vec<(Field, String)>,
    pub warnings: Vec<(Field, String)>,
}

impl Messages {
    pub fn from_issues(issues: &[Issue]) -> Messages {
        let mut m = Messages::default();
        for i in issues {
            let field = field_for(&i.path);
            let list = if i.is_error() { &mut m.errors } else { &mut m.warnings };
            if !list.iter().any(|(f, _)| *f == field) {
                list.push((field, friendly(i)));
            }
        }
        m
    }

    pub fn error(&self, f: Field) -> String {
        self.errors
            .iter()
            .find(|(x, _)| *x == f)
            .map(|(_, s)| s.clone())
            .unwrap_or_default()
    }

    pub fn warning(&self, f: Field) -> String {
        self.warnings
            .iter()
            .find(|(x, _)| *x == f)
            .map(|(_, s)| s.clone())
            .unwrap_or_default()
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// "= 10.0 clicks/sec".
pub fn cps_text(interval_ms: i32) -> String {
    if interval_ms <= 0 {
        return String::new();
    }
    let cps = 1000.0 / f64::from(interval_ms);
    if cps >= 100.0 {
        format!("= {cps:.0} clicks/sec")
    } else if cps >= 1.0 {
        format!("= {cps:.1} clicks/sec")
    } else {
        format!("= 1 click every {:.1} s", f64::from(interval_ms) / 1000.0)
    }
}

/// A short name for the picked game window, e.g. "Sober" for
/// `org.vinegarhq.Sober`.
pub fn window_name(rule: &TargetRule) -> String {
    let class = rule.window_class.trim();
    if !class.is_empty() {
        let last = class.rsplit('.').next().unwrap_or(class);
        let mut chars = last.chars();
        return match chars.next() {
            Some(c) => c.to_uppercase().chain(chars).collect(),
            None => class.to_owned(),
        };
    }
    let title = rule.window_title_pattern.trim();
    if title.is_empty() {
        String::new()
    } else {
        format!("Title contains “{title}”")
    }
}

/// Whether a window reported by "Pick window" is Pulse's own window.
pub fn is_own_window(w: &WindowInfo) -> bool {
    let own = |s: &str| s.eq_ignore_ascii_case("pulse-ui") || s.eq_ignore_ascii_case("pulse");
    own(&w.class) || own(&w.instance)
}

/// "Last run: 42 clicks · stopped by user".
pub fn last_run_text(s: &StopInfo, unit_is_key: bool) -> String {
    let noun = match (s.clicks, unit_is_key) {
        (1, false) => "click",
        (_, false) => "clicks",
        (1, true) => "key press",
        (_, true) => "key presses",
    };
    let why = match s.reason {
        StopReason::UserStop => "stopped",
        StopReason::EmergencyStop => "emergency stop",
        StopReason::TriggerReleased => "hotkey released",
        StopReason::Completed => "click limit reached",
        StopReason::DurationElapsed => "time limit reached",
        StopReason::FocusLost => "game window lost focus",
        StopReason::Suspend => "stopped after sleep",
        StopReason::BackendFault => "input problem",
        StopReason::X11ConnectionLost => "display connection lost",
        StopReason::Shutdown => "Pulse closed",
    };
    format!("Last run: {} {noun} · {why}", s.clicks)
}

/// Short wording for messages the engine pushes (e.g. a rejected hotkey),
/// so they fit the window's one-line message slot.
pub fn friendly_fault(message: &str) -> String {
    if message.contains("not in front") {
        "Ignored: the game window isn't in front.".into()
    } else if message.starts_with("Pick the game window") {
        "Ignored: pick the game window first.".into()
    } else if message.starts_with("Fix the settings") {
        "Ignored: fix the settings first.".into()
    } else if message == "Already clicking" {
        "Already clicking.".into()
    } else {
        message.to_owned()
    }
}

/// "123 clicks" / "1 key press".
pub fn count_text(n: u64, unit_is_key: bool) -> String {
    match (n, unit_is_key) {
        (1, false) => "1 click".into(),
        (n, false) => format!("{n} clicks"),
        (1, true) => "1 key press".into(),
        (n, true) => format!("{n} key presses"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_core::Pressable;

    #[test]
    fn default_config_roundtrips_through_the_form() {
        let cfg = Config::default();
        let f = form_from_config(&cfg);
        assert_eq!(f.click_index, 0);
        assert_eq!(f.key, "E");
        assert_eq!((f.interval_ms, f.hold_ms, f.mode_index), (100, 20, 0));
        assert_eq!(f.hotkey, "F6");
        assert!(f.require_focus && !f.advanced);
        assert_eq!(apply_form(&cfg, &f).expect("valid"), cfg);
    }

    #[test]
    fn form_changes_map_to_config() {
        let base = Config::default();
        let mut f = form_from_config(&base);
        f.click_index = KEYBOARD_INDEX;
        f.key = "Space".into();
        f.interval_ms = 50;
        f.hold_ms = 10;
        f.mode_index = 1;
        f.hotkey = "F7".into();
        f.count = 25;
        f.duration_s = 30;
        f.advanced = true;
        f.require_focus = false;
        let cfg = apply_form(&base, &f).expect("valid");
        assert_eq!(cfg.clicker.target(), Pressable::Key(Key::parse("Space").expect("key")));
        assert_eq!((cfg.clicker.interval_ms, cfg.clicker.hold_ms), (50, 10));
        assert_eq!(cfg.clicker.activation_mode, ActivationMode::Hold);
        assert_eq!(cfg.clicker.activation_hotkey.name(), "F7");
        assert_eq!((cfg.clicker.count, cfg.clicker.duration_ms), (25, 30_000));
        assert!(cfg.engine.allow_advanced_rates);
        assert!(!cfg.target.require_focus);
        // And back.
        assert_eq!(form_from_config(&cfg), f);

        f.click_index = 1;
        let cfg = apply_form(&base, &f).expect("valid");
        assert_eq!(cfg.clicker.target(), Pressable::Button(MouseButton::Right));
        assert_eq!(cfg.clicker.key.name(), "Space", "the key choice is remembered");
    }

    #[test]
    fn non_whole_second_durations_are_kept_unless_edited() {
        let mut base = Config::default();
        base.clicker.duration_ms = 1_500;
        let f = form_from_config(&base);
        assert_eq!(f.duration_s, 2);
        assert_eq!(apply_form(&base, &f).expect("valid").clicker.duration_ms, 1_500);
        let mut f2 = f.clone();
        f2.duration_s = 3;
        assert_eq!(apply_form(&base, &f2).expect("valid").clicker.duration_ms, 3_000);
    }

    #[test]
    fn unknown_key_names_are_form_errors() {
        let mut f = form_from_config(&Config::default());
        f.key = "NotAKey".into();
        let e = apply_form(&Config::default(), &f).expect_err("unknown key");
        assert_eq!(e.field, Field::Key);
        f.key = "E".into();
        f.hotkey = String::new();
        assert_eq!(
            apply_form(&Config::default(), &f).expect_err("empty").field,
            Field::Hotkey
        );
    }

    #[test]
    fn issue_paths_map_to_fields_with_friendly_text() {
        let mut cfg = Config::default();
        cfg.clicker.interval_ms = 15;
        cfg.clicker.hold_ms = 20;
        let err = pulse_config::plan(&cfg).expect_err("hold >= interval");
        let m = Messages::from_issues(&err.issues);
        assert_eq!(m.error(Field::Hold), "Must be shorter than the speed.");
        assert!(m.warning(Field::Speed).contains("Roblox chat"));
        assert!(m.has_errors());

        cfg.clicker.interval_ms = 5;
        cfg.clicker.hold_ms = 2;
        let err = pulse_config::plan(&cfg).expect_err("below minimum");
        let m = Messages::from_issues(&err.issues);
        assert!(m.error(Field::Speed).contains("Allow very fast speeds"));

        let mut cfg = Config::default();
        cfg.clicker.activation_hotkey = Key::F8;
        let err = pulse_config::plan(&cfg).expect_err("same as emergency");
        assert_eq!(
            Messages::from_issues(&err.issues).error(Field::Hotkey),
            "Can't be the emergency stop key."
        );

        assert_eq!(field_for("target"), Field::Window);
        assert_eq!(field_for("clicker.count"), Field::Count);
        assert_eq!(field_for("clicker.duration_ms"), Field::Duration);
        assert_eq!(field_for("schema_version"), Field::General);
    }

    #[test]
    fn engine_faults_are_shortened() {
        assert_eq!(
            friendly_fault("The game window is not in front: switch to it and press the hotkey"),
            "Ignored: the game window isn't in front."
        );
        assert_eq!(
            friendly_fault("Pick the game window first"),
            "Ignored: pick the game window first."
        );
        assert_eq!(friendly_fault("something else"), "something else");
    }

    #[test]
    fn display_texts() {
        assert_eq!(cps_text(100), "= 10.0 clicks/sec");
        assert_eq!(cps_text(2), "= 500 clicks/sec");
        assert_eq!(cps_text(2000), "= 1 click every 2.0 s");
        assert_eq!(cps_text(0), "");
        let rule = TargetRule {
            window_class: "org.vinegarhq.Sober".into(),
            window_title_pattern: String::new(),
        };
        assert_eq!(window_name(&rule), "Sober");
        assert_eq!(
            window_name(&TargetRule {
                window_class: "firefox".into(),
                window_title_pattern: String::new()
            }),
            "Firefox"
        );
        assert_eq!(window_name(&TargetRule::default()), "");
        assert_eq!(count_text(1, false), "1 click");
        assert_eq!(count_text(3, true), "3 key presses");
        let stop = StopInfo {
            run_id: 1,
            reason: StopReason::FocusLost,
            clicks: 42,
            events: 84,
            missed_deadlines: 0,
            duration_ms: 4200,
            release_ok: true,
        };
        assert_eq!(
            last_run_text(&stop, false),
            "Last run: 42 clicks · game window lost focus"
        );
        assert!(is_own_window(&WindowInfo {
            class: "pulse-ui".into(),
            ..WindowInfo::default()
        }));
    }
}
