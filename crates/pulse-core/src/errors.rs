//! Core error and validation-issue types.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
}

/// A single validation finding, addressed by a field path such as
/// `actions[3].hold_ms` so the UI can point at the offending step and field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub severity: Severity,
    pub path: String,
    pub message: String,
}

impl Issue {
    pub fn error(path: impl Into<String>, message: impl Into<String>) -> Self {
        Issue {
            severity: Severity::Error,
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn warning(path: impl Into<String>, message: impl Into<String>) -> Self {
        Issue {
            severity: Severity::Warning,
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        if self.path.is_empty() {
            write!(f, "{sev}: {}", self.message)
        } else {
            write!(f, "{sev}: {}: {}", self.path, self.message)
        }
    }
}

/// Validation failed with at least one error. Warnings are included too.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub struct ValidationFailed {
    pub issues: Vec<Issue>,
}

impl fmt::Display for ValidationFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let errors: Vec<String> = self
            .issues
            .iter()
            .filter(|i| i.is_error())
            .map(|i| i.to_string())
            .collect();
        write!(f, "validation failed: {}", errors.join("; "))
    }
}

impl ValidationFailed {
    pub fn single(path: impl Into<String>, message: impl Into<String>) -> Self {
        ValidationFailed {
            issues: vec![Issue::error(path, message)],
        }
    }
}
