//! Target-window rules and the focus guard.
//!
//! No window class is hardcoded: rules come from the user's explicit window
//! selection. Matching is case-insensitive on the WM_CLASS instance or class
//! and an optional title substring, so a target survives relaunches and title
//! changes.

use serde::{Deserialize, Serialize};

/// Metadata of an X11 top-level client window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: u32,
    /// WM_CLASS instance part.
    pub instance: String,
    /// WM_CLASS class part.
    pub class: String,
    pub title: String,
    /// `_NET_WM_PID`, informational only (may be a sandbox-namespace PID).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TargetRule {
    /// Matches either the WM_CLASS instance or class, case-insensitively.
    pub window_class: String,
    /// Case-insensitive substring of the window title; empty = any title.
    pub window_title_pattern: String,
}

impl TargetRule {
    pub fn is_empty(&self) -> bool {
        self.window_class.trim().is_empty() && self.window_title_pattern.trim().is_empty()
    }

    /// Build a rule from a selected window: class first, title only as a
    /// fallback when the window has no WM_CLASS.
    pub fn from_window(w: &WindowInfo) -> TargetRule {
        if !w.class.is_empty() {
            TargetRule {
                window_class: w.class.clone(),
                window_title_pattern: String::new(),
            }
        } else if !w.instance.is_empty() {
            TargetRule {
                window_class: w.instance.clone(),
                window_title_pattern: String::new(),
            }
        } else {
            TargetRule {
                window_class: String::new(),
                window_title_pattern: w.title.clone(),
            }
        }
    }

    pub fn matches(&self, w: &WindowInfo) -> bool {
        if self.is_empty() {
            return false;
        }
        let class = self.window_class.trim();
        if !class.is_empty() && !(w.class.eq_ignore_ascii_case(class) || w.instance.eq_ignore_ascii_case(class)) {
            return false;
        }
        let pat = self.window_title_pattern.trim();
        if !pat.is_empty() && !w.title.to_lowercase().contains(&pat.to_lowercase()) {
            return false;
        }
        true
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.window_class.len() > 256 || self.window_title_pattern.len() > 256 {
            return Err("target rule fields are limited to 256 characters".into());
        }
        if self.window_class.chars().any(char::is_control) || self.window_title_pattern.chars().any(char::is_control) {
            return Err("target rule fields must not contain control characters".into());
        }
        Ok(())
    }
}

/// Tracks whether the currently active window satisfies the target rule.
#[derive(Clone, Debug, Default)]
pub struct FocusGuard {
    rule: TargetRule,
    active: Option<WindowInfo>,
    matched: bool,
}

impl FocusGuard {
    pub fn new(rule: TargetRule) -> Self {
        FocusGuard {
            rule,
            active: None,
            matched: false,
        }
    }

    pub fn rule(&self) -> &TargetRule {
        &self.rule
    }

    pub fn active(&self) -> Option<&WindowInfo> {
        self.active.as_ref()
    }

    pub fn set_rule(&mut self, rule: TargetRule) {
        self.rule = rule;
        self.recompute();
    }

    /// Record a focus change. Returns true when the match status changed.
    pub fn set_active(&mut self, w: Option<WindowInfo>) -> bool {
        self.active = w;
        let before = self.matched;
        self.recompute();
        before != self.matched
    }

    fn recompute(&mut self) {
        self.matched = self.active.as_ref().is_some_and(|w| self.rule.matches(w));
    }

    pub fn target_focused(&self) -> bool {
        self.matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(class: &str, title: &str) -> WindowInfo {
        WindowInfo {
            id: 42,
            instance: class.to_lowercase(),
            class: class.to_owned(),
            title: title.to_owned(),
            pid: None,
        }
    }

    #[test]
    fn empty_rule_never_matches() {
        assert!(!TargetRule::default().matches(&win("Anything", "x")));
    }

    #[test]
    fn class_and_title_matching() {
        let r = TargetRule {
            window_class: "org.vinegarhq.sober".into(),
            window_title_pattern: String::new(),
        };
        assert!(r.matches(&win("org.vinegarhq.Sober", "Roblox")));
        assert!(!r.matches(&win("firefox", "Roblox")));

        let r = TargetRule {
            window_class: String::new(),
            window_title_pattern: "roblox".into(),
        };
        assert!(r.matches(&win("x", "Sober — ROBLOX")));
        assert!(!r.matches(&win("x", "Terminal")));
    }

    #[test]
    fn guard_reports_changes_on_focus_transitions() {
        let rule = TargetRule::from_window(&win("Sober", "Roblox"));
        let mut g = FocusGuard::new(rule);
        assert!(!g.target_focused());
        assert!(g.set_active(Some(win("Sober", "Roblox (changed title)"))));
        assert!(g.target_focused());
        assert!(g.set_active(Some(win("code", "editor"))));
        assert!(!g.target_focused());
        assert!(!g.set_active(None));
    }

    #[test]
    fn rule_validation() {
        let r = TargetRule {
            window_class: "a\nb".into(),
            window_title_pattern: String::new(),
        };
        assert!(r.validate().is_err());
    }
}
