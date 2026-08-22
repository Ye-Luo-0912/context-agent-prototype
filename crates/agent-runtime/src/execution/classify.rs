//! E2E `fs.read` motive: why the model needed to read this path.

use agent_contracts::{
    FS_READ_MOTIVE_KEY, FsReadMotive, FsRereadClass, ResourceFreshness, ToolOutput,
};

use super::state::{ResourceFact, ResourceProvenance};

/// Combine last-prompt exposure with the prior resource fact.
///
/// Priority answers "why this read":
/// 1. `changed` — digest actually moved (legitimate).
/// 2. `warm` / `stored` — GC dropped the body.
/// 3. `protocol-checkpoint-body-missing` — body consumed earlier, digest
///    unchanged, frame now identity-only (SCHED-04 instrument).
/// 4. `needs-revalidation` — Runtime should have hashed instead.
/// 5. `body-visible-current` — file body was in the last prompt.
/// 6. `descriptor-only` — last prompt only had `path@rev`.
/// 7. `checked-fresh` — Runtime already knew `path@rev`.
/// 8. `first` — first exploration.
pub fn classify_fs_read_motive(
    residency: FsRereadClass,
    prior: Option<&ResourceFact>,
    new_digest: Option<&str>,
) -> FsReadMotive {
    let new_digest = new_digest.filter(|digest| !digest.is_empty());
    if let Some(fact) = prior {
        let old = (!fact.digest.is_empty()).then_some(fact.digest.as_str());
        if let (Some(old), Some(new)) = (old, new_digest)
            && old != new
        {
            return FsReadMotive::Changed;
        }
    }
    match residency {
        FsRereadClass::Warm => return FsReadMotive::Warm,
        FsRereadClass::Stored => return FsReadMotive::Stored,
        FsRereadClass::FirstRead
        | FsRereadClass::PreviouslySelected
        | FsRereadClass::SelectedDescriptor
        | FsRereadClass::ExternalDescriptor
        | FsRereadClass::ResidentUnselected => {}
    }
    // SCHED-04 instrument: identity-only exposure of a body the model
    // already consumed (read-provenance fact, unchanged digest). Only
    // descriptor residency qualifies — ResidentUnselected means the
    // engine still holds the body (a packing choice, not a loss), and
    // FirstRead means nothing was ever materialized for a cache to serve.
    if let Some(fact) = prior {
        let unchanged = (!fact.digest.is_empty())
            .then_some(fact.digest.as_str())
            .zip(new_digest)
            .is_some_and(|(old, new)| old == new);
        if unchanged
            && fact.provenance == ResourceProvenance::Read
            && matches!(
                residency,
                FsRereadClass::SelectedDescriptor | FsRereadClass::ExternalDescriptor
            )
        {
            return FsReadMotive::ProtocolCheckpointBodyMissing;
        }
    }
    if let Some(fact) = prior {
        if fact.freshness == ResourceFreshness::NeedsRevalidation {
            return FsReadMotive::NeedsRevalidation;
        }
        if fact.freshness == ResourceFreshness::Fresh {
            return match residency {
                FsRereadClass::PreviouslySelected => FsReadMotive::BodyVisibleCurrent,
                FsRereadClass::SelectedDescriptor | FsRereadClass::ExternalDescriptor => {
                    FsReadMotive::DescriptorOnly
                }
                FsRereadClass::FirstRead
                | FsRereadClass::ResidentUnselected
                | FsRereadClass::Warm
                | FsRereadClass::Stored => FsReadMotive::CheckedFresh,
            };
        }
    }
    match residency {
        FsRereadClass::PreviouslySelected => FsReadMotive::BodyVisibleCurrent,
        FsRereadClass::SelectedDescriptor | FsRereadClass::ExternalDescriptor => {
            FsReadMotive::DescriptorOnly
        }
        FsRereadClass::ResidentUnselected | FsRereadClass::FirstRead => FsReadMotive::First,
        FsRereadClass::Warm => FsReadMotive::Warm,
        FsRereadClass::Stored => FsReadMotive::Stored,
    }
}

pub fn stamp_fs_read_motive(output: &mut ToolOutput, motive: FsReadMotive) {
    if !output.metadata.is_object() {
        output.metadata = serde_json::json!({});
    }
    if let Some(object) = output.metadata.as_object_mut() {
        object.insert(
            FS_READ_MOTIVE_KEY.to_string(),
            serde_json::Value::String(motive.as_str().to_string()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ResourceProvenance;
    use agent_contracts::ResourceFreshness;

    fn fact(digest: &str, freshness: ResourceFreshness) -> ResourceFact {
        ResourceFact {
            path: "src/util.py".into(),
            digest: digest.into(),
            freshness,
            turn: 1,
            provenance: ResourceProvenance::Read,
        }
    }

    #[test]
    fn same_hash_after_unknown_is_checked_fresh_not_first() {
        // Engine has not ingested this turn's body yet, so residency is
        // first-read; Runtime still holds util.py@B.
        let motive = classify_fs_read_motive(
            FsRereadClass::FirstRead,
            Some(&fact("revB", ResourceFreshness::Fresh)),
            Some("revB"),
        );
        assert_eq!(motive, FsReadMotive::CheckedFresh);
    }

    #[test]
    fn body_in_last_prompt_is_body_visible_current() {
        let motive = classify_fs_read_motive(
            FsRereadClass::PreviouslySelected,
            Some(&fact("revB", ResourceFreshness::Fresh)),
            Some("revB"),
        );
        assert_eq!(motive, FsReadMotive::BodyVisibleCurrent);
    }

    #[test]
    fn selected_descriptor_without_a_body_read_stays_descriptor_only() {
        // Mutation-stamp provenance: the runtime knows path@rev but no
        // body was ever consumed, so a body cache has nothing to serve.
        let mut never_read = fact("revB", ResourceFreshness::Fresh);
        never_read.provenance = ResourceProvenance::MutationResult;
        let motive = classify_fs_read_motive(
            FsRereadClass::SelectedDescriptor,
            Some(&never_read),
            Some("revB"),
        );
        assert_eq!(motive, FsReadMotive::DescriptorOnly);
    }

    #[test]
    fn descriptor_exposure_of_a_consumed_body_is_protocol_checkpoint_missing() {
        // The model read this exact body earlier; the frame now carries
        // identity only (SCHED-04). Both descriptor classes qualify.
        for residency in [
            FsRereadClass::SelectedDescriptor,
            FsRereadClass::ExternalDescriptor,
        ] {
            let motive = classify_fs_read_motive(
                residency,
                Some(&fact("revB", ResourceFreshness::Fresh)),
                Some("revB"),
            );
            assert_eq!(motive, FsReadMotive::ProtocolCheckpointBodyMissing);
        }
    }

    #[test]
    fn pending_revalidation_descriptor_of_a_consumed_body_is_protocol_checkpoint_missing() {
        // An unknown-footprint boundary flips facts to needs-revalidation,
        // but with an unchanged digest and read provenance the frame still
        // lost a body the cache could have served.
        let motive = classify_fs_read_motive(
            FsRereadClass::SelectedDescriptor,
            Some(&fact("revB", ResourceFreshness::NeedsRevalidation)),
            Some("revB"),
        );
        assert_eq!(motive, FsReadMotive::ProtocolCheckpointBodyMissing);
    }

    #[test]
    fn external_descriptor_is_descriptor_only() {
        let motive = classify_fs_read_motive(FsRereadClass::ExternalDescriptor, None, Some("revB"));
        assert_eq!(motive, FsReadMotive::DescriptorOnly);
    }

    #[test]
    fn gc_warm_wins_over_fresh_identity() {
        let motive = classify_fs_read_motive(
            FsRereadClass::Warm,
            Some(&fact("revB", ResourceFreshness::Fresh)),
            Some("revB"),
        );
        assert_eq!(motive, FsReadMotive::Warm);
    }

    #[test]
    fn digest_change_is_changed() {
        let motive = classify_fs_read_motive(
            FsRereadClass::PreviouslySelected,
            Some(&fact("revA", ResourceFreshness::Fresh)),
            Some("revB"),
        );
        assert_eq!(motive, FsReadMotive::Changed);
    }

    #[test]
    fn pending_revalidation_without_gc_is_needs_revalidation() {
        let motive = classify_fs_read_motive(
            FsRereadClass::ResidentUnselected,
            Some(&fact("revB", ResourceFreshness::NeedsRevalidation)),
            Some("revB"),
        );
        assert_eq!(motive, FsReadMotive::NeedsRevalidation);
    }

    #[test]
    fn no_fact_first_read_is_first() {
        let motive = classify_fs_read_motive(FsRereadClass::FirstRead, None, Some("revA"));
        assert_eq!(motive, FsReadMotive::First);
    }

    #[test]
    fn old_selected_current_wire_name_still_parses() {
        assert_eq!(
            FsReadMotive::parse("selected-current"),
            Some(FsReadMotive::BodyVisibleCurrent)
        );
        assert_eq!(
            FsReadMotive::parse("body-visible-current"),
            Some(FsReadMotive::BodyVisibleCurrent)
        );
    }
}
