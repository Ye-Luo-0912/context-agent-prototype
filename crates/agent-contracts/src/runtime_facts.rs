//! Bounded, system-owned Runtime Facts (`TOOL-ENV-01`).
//!
//! The trusted composition root captures this profile. `PromptAssembler`
//! renders it as a stable system block after System Policy. It never enters
//! `ContextEngine`, transcript history, or GC.

use serde::{Deserialize, Serialize};

/// Wire schema id for the model-facing facts block.
pub const RUNTIME_FACTS_SCHEMA: &str = "runtime_facts/v1";
/// Hard cap on the rendered UTF-8 facts block.
pub const RUNTIME_FACTS_MAX_BYTES: usize = 1024;
/// At most this many workspace markers appear in the block.
pub const RUNTIME_FACTS_MAX_MARKERS: usize = 16;
/// Each marker is truncated to this many UTF-8 bytes.
pub const RUNTIME_FACTS_MAX_MARKER_BYTES: usize = 64;

/// Normalized host/workspace facts charged as a fixed prompt layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeFactsView {
    pub platform: String,
    pub architecture: String,
    pub markers: Vec<String>,
    /// Bumped only when workspace markers change after a committed mutation.
    pub revision: u64,
}

impl RuntimeFactsView {
    pub fn new(
        platform: impl Into<String>,
        architecture: impl Into<String>,
        markers: Vec<String>,
    ) -> Self {
        Self {
            platform: sanitize_fact_token(&platform.into(), 64),
            architecture: sanitize_fact_token(&architecture.into(), 32),
            markers: bound_markers(markers),
            revision: 0,
        }
    }

    /// Replace the marker list. OS identity stays immutable; revision bumps
    /// only when the bounded marker set actually changes.
    pub fn set_markers(&mut self, markers: Vec<String>) {
        let markers = bound_markers(markers);
        if markers != self.markers {
            self.markers = markers;
            self.revision = self.revision.saturating_add(1);
        }
    }

    /// Model-facing block. Cache-stable for a given platform/arch/markers.
    /// The revision is not rendered so provider prompt caches stay warm.
    pub fn render(&self) -> String {
        let markers = if self.markers.is_empty() {
            "(none)".to_string()
        } else {
            format!("[{}]", self.markers.join(", "))
        };
        let text = format!(
            "{RUNTIME_FACTS_SCHEMA}\nplatform: {}\narchitecture: {}\nworkspace: relative paths; markers: {markers}",
            self.platform, self.architecture
        );
        bound_utf8(&text, RUNTIME_FACTS_MAX_BYTES).to_string()
    }
}

/// Keep a known-marker name inside the per-marker byte cap.
pub fn bound_marker(name: &str) -> String {
    sanitize_fact_token(name, RUNTIME_FACTS_MAX_MARKER_BYTES)
}

pub fn bound_markers(markers: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = markers
        .into_iter()
        .map(|marker| bound_marker(&marker))
        .filter(|marker| !marker.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out.truncate(RUNTIME_FACTS_MAX_MARKERS);
    out
}

fn sanitize_fact_token(raw: &str, max_bytes: usize) -> String {
    let cleaned: String = raw
        .chars()
        .map(|ch| {
            if ch.is_control() || ch == '/' || ch == '\\' {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded = bound_utf8(&cleaned, max_bytes);
    if bounded.is_empty() {
        "unknown".into()
    } else {
        bounded.to_string()
    }
}

fn bound_utf8(value: &str, max_bytes: usize) -> &str {
    let mut end = value.len().min(max_bytes);
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_capped_and_omits_revision() {
        let facts = RuntimeFactsView::new("windows 11", "x86_64", vec!["Cargo.toml".into()]);
        let rendered = facts.render();
        assert!(rendered.starts_with(RUNTIME_FACTS_SCHEMA));
        assert!(rendered.contains("platform: windows 11"));
        assert!(rendered.contains("architecture: x86_64"));
        assert!(rendered.contains("markers: [Cargo.toml]"));
        assert!(!rendered.contains("revision"));
        assert!(rendered.len() <= RUNTIME_FACTS_MAX_BYTES);
    }

    #[test]
    fn empty_markers_are_explicit() {
        let facts = RuntimeFactsView::new("ubuntu 24.04", "aarch64", Vec::new());
        assert!(facts.render().contains("markers: (none)"));
    }

    #[test]
    fn markers_are_sorted_deduped_and_capped() {
        let mut oversized = Vec::new();
        for i in 0..20 {
            oversized.push(format!("m{i:02}"));
        }
        oversized.push("m00".into());
        let facts = RuntimeFactsView::new("unknown", "unknown", oversized);
        assert_eq!(facts.markers.len(), RUNTIME_FACTS_MAX_MARKERS);
        let mut sorted = facts.markers.clone();
        sorted.sort();
        assert_eq!(facts.markers, sorted);
    }

    #[test]
    fn set_markers_bumps_revision_only_on_change() {
        let mut facts = RuntimeFactsView::new("windows 11", "x86_64", vec![".git".into()]);
        facts.set_markers(vec![".git".into()]);
        assert_eq!(facts.revision, 0);
        facts.set_markers(vec![".git".into(), "Cargo.toml".into()]);
        assert_eq!(facts.revision, 1);
        assert_eq!(facts.markers, vec![".git", "Cargo.toml"]);
    }

    #[test]
    fn sanitizes_paths_and_control_chars() {
        let facts = RuntimeFactsView::new(
            "windows\n11/secret",
            "x86_64",
            vec!["C:\\Users\\x\\Cargo.toml".into()],
        );
        assert_eq!(facts.platform, "windows 11 secret");
        assert!(!facts.render().contains('\\'));
        assert!(!facts.markers.iter().any(|m| m.contains('/')));
    }
}
