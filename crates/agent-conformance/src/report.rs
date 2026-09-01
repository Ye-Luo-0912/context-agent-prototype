//! Conformance report: a bounded, renderable list of violations.

use std::fmt;

/// One failed contract check: the subject (tool name or a surface rule),
/// the check family, and a human-readable explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConformanceViolation {
    /// Tool name, `schema:<name>`, or `surface`.
    pub subject: String,
    /// The check family: `schema`, `output`, `error`, `surface`.
    pub check: &'static str,
    pub message: String,
}

impl ConformanceViolation {
    pub fn new(
        subject: impl Into<String>,
        check: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            subject: subject.into(),
            check,
            message: message.into(),
        }
    }
}

impl fmt::Display for ConformanceViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}: {}", self.check, self.subject, self.message)
    }
}

/// The aggregate result of running one or more checks against a dispatcher.
#[derive(Debug, Clone, Default)]
pub struct ConformanceReport {
    pub subjects_checked: usize,
    pub violations: Vec<ConformanceViolation>,
    /// Evaluated surface/schema digest (SHA-256 hex, see
    /// [`crate::checks::surface_digest`]). Filled by inventory-parity
    /// evaluation so the host can persist it and detect surface drift.
    pub surface_digest: Option<String>,
}

impl ConformanceReport {
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }

    pub fn push(&mut self, violation: ConformanceViolation) {
        self.violations.push(violation);
    }

    pub fn extend(&mut self, violations: impl IntoIterator<Item = ConformanceViolation>) {
        self.violations.extend(violations);
    }

    /// One line per violation, plus a verdict — bounded and deterministic
    /// so CI output is readable.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "conformance: {} subjects checked, {} violations\n",
            self.subjects_checked,
            self.violations.len()
        ));
        for violation in &self.violations {
            out.push_str(&format!("  {violation}\n"));
        }
        out.push_str(if self.is_clean() {
            "verdict: clean\n"
        } else {
            "verdict: FAIL\n"
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_renders_verdict_and_violations() {
        let mut report = ConformanceReport {
            subjects_checked: 2,
            ..ConformanceReport::default()
        };
        report.push(ConformanceViolation::new(
            "fs.read",
            "output",
            "summary exceeds the cap",
        ));
        let rendered = report.render();
        assert!(rendered.contains("verdict: FAIL"));
        assert!(rendered.contains("fs.read"));

        let clean = ConformanceReport {
            subjects_checked: 2,
            violations: Vec::new(),
            surface_digest: None,
        };
        assert!(clean.is_clean());
        assert!(clean.render().contains("verdict: clean"));
    }
}
