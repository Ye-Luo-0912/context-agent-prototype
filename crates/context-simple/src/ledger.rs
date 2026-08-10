//! The artifact-backed lifecycle ledger: a bounded, in-engine buffer of
//! item lifecycle rows (`item` / `revision` / `axis` / `from` / `to` /
//! `cause` / `trigger` / `turn` / `related-id`) that is exported to a
//! JSONL artifact on demand. Rows are recorded where transitions already
//! exist (maintenance, GC, scope close, directives); this module only
//! projects, bounds and exports them. Export is explicit and never on the
//! context hot path — the buffer itself is cheap (a bounded Vec append).

use agent_contracts::{ContextItemId, ContextLifecycleRecord, LifecycleAxis};

use crate::engine::State;

/// Append one lifecycle row for an item. The row carries the cause and the
/// trigger exactly as the transition/eviction site produced them, plus the
/// user-turn and event-sequence clocks — so replay and post-mortem can
/// answer "entered/selected/cooled/evicted/reactivated because ..., at
/// turn N, triggered by X". Eight positional args mirror the record's
/// fields 1:1; the sites that call this already hold all of them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    state: &mut State,
    item_id: ContextItemId,
    axis: LifecycleAxis,
    from: impl Into<String>,
    to: impl Into<String>,
    cause: impl Into<String>,
    trigger: impl Into<String>,
    related_id: Option<ContextItemId>,
) {
    let revision = state.ledger_revisions.entry(item_id).or_insert(0);
    *revision += 1;
    let revision = *revision;
    state.ledger.push(ContextLifecycleRecord {
        item_id,
        revision,
        axis,
        from: from.into(),
        to: to.into(),
        cause: cause.into(),
        trigger: trigger.into(),
        turn: state.turn,
        related_id,
        event_seq: state.event_seq,
    });
    let cap = state.ledger_cap.max(1);
    if state.ledger.len() > cap {
        let overflow = state.ledger.len() - cap;
        state.ledger.drain(..overflow);
    }
}

/// JSONL-serialize the buffered rows (one record per line) without touching
/// the engine state; `export_ledger` owns the file write.
pub(crate) fn encode(rows: &[ContextLifecycleRecord]) -> String {
    let mut out = String::new();
    for record in rows {
        if let Ok(line) = serde_json::to_string(record) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::State;

    #[test]
    fn ledger_is_bounded_and_revisions_are_per_item() {
        let mut state = State {
            ledger_cap: 4,
            ..State::default()
        };
        let a = ContextItemId::new();
        let b = ContextItemId::new();
        for _ in 0..3 {
            record(
                &mut state,
                a,
                LifecycleAxis::Attention,
                "Active",
                "Cooling",
                "decayed",
                "maintain",
                None,
            );
            record(
                &mut state,
                b,
                LifecycleAxis::Attention,
                "Active",
                "Cooling",
                "decayed",
                "maintain",
                None,
            );
        }
        // 6 rows into a cap of 4: the two oldest (a1, b1) drop, the newest
        // survive in insertion order (a2, b2, a3, b3).
        assert_eq!(state.ledger.len(), 4, "the ledger stays bounded");
        assert_eq!(
            state.ledger[0].item_id, a,
            "the first survivor is a's second row"
        );
        assert_eq!(
            state.ledger[0].revision, 2,
            "a's second row carries revision 2"
        );
        let rows_for_a: Vec<u64> = state
            .ledger
            .iter()
            .filter(|r| r.item_id == a)
            .map(|r| r.revision)
            .collect();
        assert_eq!(
            rows_for_a,
            vec![2, 3],
            "a's surviving rows keep revisions 2, 3"
        );
        let rows_for_b: Vec<u64> = state
            .ledger
            .iter()
            .filter(|r| r.item_id == b)
            .map(|r| r.revision)
            .collect();
        assert_eq!(
            rows_for_b,
            vec![2, 3],
            "b's surviving rows keep revisions 2, 3"
        );
        // The dropped rows are gone, but their revision counters survive so
        // the next row for `a` continues at 4.
        record(
            &mut state,
            a,
            LifecycleAxis::Attention,
            "Cooling",
            "Active",
            "promoted",
            "maintain",
            None,
        );
        let last = state.ledger.last().expect("a row was added");
        assert_eq!(last.revision, 4, "revisions are monotonic per item");
    }

    #[test]
    fn encode_emits_one_json_line_per_row() {
        let mut state = State {
            ledger_cap: 8,
            ..State::default()
        };
        record(
            &mut state,
            ContextItemId::new(),
            LifecycleAxis::Semantic,
            "Live",
            "Tombstoned",
            "ephemeral TTL expired",
            "maintain",
            None,
        );
        let text = encode(&state.ledger);
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("Tombstoned"));
        assert!(text.contains("\"axis\":\"semantic\""));
    }

    #[test]
    fn checkpoint_roundtrip_preserves_the_ledger() {
        let mut state = State {
            ledger_cap: 8,
            ..State::default()
        };
        let id = ContextItemId::new();
        record(
            &mut state,
            id,
            LifecycleAxis::Gc,
            "Resident",
            "Warm",
            "unreachable from roots",
            "gc",
            None,
        );
        let json = serde_json::to_string(&state).unwrap();
        let restored: State = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.ledger.len(), 1);
        assert_eq!(restored.ledger[0].item_id, id);
        assert_eq!(restored.ledger_revisions.get(&id), Some(&1));
    }
}
