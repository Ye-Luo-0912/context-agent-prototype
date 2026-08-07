//! Typed item labels. Scope promotion and GC decide membership by enum
//! values instead of matching raw strings, so a misspelled tag can no longer
//! silently change lifecycle behavior. Extension namespaces (`ext:github/pr`)
//! stay open for modules that need their own labels.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Content labels the core context lifecycle understands. Promotion and GC
/// use these to decide what survives a scope close and what is excluded from
/// model requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoreLabel {
    Decision,
    Finding,
    Constraint,
    OpenLoop,
    ArtifactRef,
    EvidenceRef,
}

impl CoreLabel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Finding => "finding",
            Self::Constraint => "constraint",
            Self::OpenLoop => "open-loop",
            Self::ArtifactRef => "artifact-ref",
            Self::EvidenceRef => "evidence-ref",
        }
    }
}

/// Lifecycle markers the core GC/promotion machinery stamps onto items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LifecycleLabel {
    /// The item was promoted to its parent scope; guards against
    /// double-promotion.
    Promoted,
    /// The decision was superseded by a later decision on the same entity.
    Superseded,
    /// The error was verified as fixed by a later success.
    VerifiedFixed,
}

impl LifecycleLabel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Promoted => "promoted",
            Self::Superseded => "superseded",
            Self::VerifiedFixed => "verified-fixed",
        }
    }
}

/// A namespaced item label: a core content label, a core lifecycle marker,
/// or an extension namespace (`ext:github/pr`). Serializes to its string
/// form and accepts any string back, so old checkpoints and extension labels
/// keep working.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Label {
    Core(CoreLabel),
    Lifecycle(LifecycleLabel),
    Extension(String),
}

impl Label {
    pub fn core(label: CoreLabel) -> Self {
        Self::Core(label)
    }

    pub fn lifecycle(label: LifecycleLabel) -> Self {
        Self::Lifecycle(label)
    }

    /// Build an extension label. The `ext:` namespace prefix is applied if
    /// the caller did not include it.
    pub fn extension(namespace: impl Into<String>) -> Self {
        let namespace = namespace.into();
        if namespace.starts_with("ext:") {
            Self::Extension(namespace)
        } else {
            Self::Extension(format!("ext:{namespace}"))
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Core(label) => label.as_str(),
            Self::Lifecycle(label) => label.as_str(),
            Self::Extension(namespace) => namespace,
        }
    }

    pub fn is_core(&self, label: CoreLabel) -> bool {
        matches!(self, Self::Core(actual) if *actual == label)
    }

    pub fn is_lifecycle(&self, label: LifecycleLabel) -> bool {
        matches!(self, Self::Lifecycle(actual) if *actual == label)
    }

    /// One of the core content labels that survives a scope close.
    pub fn is_promotable(&self) -> bool {
        matches!(
            self,
            Self::Core(
                CoreLabel::Decision
                    | CoreLabel::Finding
                    | CoreLabel::Constraint
                    | CoreLabel::OpenLoop
                    | CoreLabel::ArtifactRef
                    | CoreLabel::EvidenceRef
            )
        )
    }

    /// Parse a wire/checkpoint string back into a typed label. Known core
    /// strings map to typed variants; everything else — including extension
    /// namespaces — stays an extension label, so future labels round-trip.
    pub fn parse(text: &str) -> Self {
        match text {
            "decision" => Self::Core(CoreLabel::Decision),
            "finding" => Self::Core(CoreLabel::Finding),
            "constraint" => Self::Core(CoreLabel::Constraint),
            "open-loop" => Self::Core(CoreLabel::OpenLoop),
            "artifact-ref" => Self::Core(CoreLabel::ArtifactRef),
            "evidence-ref" => Self::Core(CoreLabel::EvidenceRef),
            "promoted" => Self::Lifecycle(LifecycleLabel::Promoted),
            "superseded" => Self::Lifecycle(LifecycleLabel::Superseded),
            "verified-fixed" => Self::Lifecycle(LifecycleLabel::VerifiedFixed),
            other => Self::Extension(other.to_string()),
        }
    }
}

impl Serialize for Label {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Label {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(Label::parse(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_and_extension_round_trip_as_strings() {
        for label in [
            Label::core(CoreLabel::Decision),
            Label::core(CoreLabel::EvidenceRef),
            Label::lifecycle(LifecycleLabel::Promoted),
            Label::extension("github/pr"),
        ] {
            let json = serde_json::to_string(&label).unwrap();
            let back: Label = serde_json::from_str(&json).unwrap();
            assert_eq!(back, label);
            assert_eq!(back.as_str(), label.as_str());
        }
        assert_eq!(
            serde_json::to_string(&Label::extension("github/pr")).unwrap(),
            "\"ext:github/pr\""
        );
        assert_eq!(
            serde_json::to_string(&Label::core(CoreLabel::Decision)).unwrap(),
            "\"decision\""
        );
    }

    #[test]
    fn legacy_strings_deserialize_typed() {
        let label: Label = serde_json::from_str("\"superseded\"").unwrap();
        assert!(label.is_lifecycle(LifecycleLabel::Superseded));
        let unknown: Label = serde_json::from_str("\"future-label\"").unwrap();
        assert_eq!(unknown.as_str(), "future-label");
    }

    #[test]
    fn promotion_membership_is_typed() {
        assert!(Label::core(CoreLabel::Decision).is_promotable());
        assert!(Label::core(CoreLabel::OpenLoop).is_promotable());
        assert!(!Label::lifecycle(LifecycleLabel::Promoted).is_promotable());
        assert!(!Label::extension("github/pr").is_promotable());
    }
}
