use std::collections::HashSet;

use agent_contracts::{ContextItem, ContextItemId, DependencyEdge};

use crate::engine::{SimpleContextConfig, State};
use crate::index::entity::entities_match;

/// Per-item cap on explicit dependency edges recorded at ingest.
pub(crate) const MAX_DEPENDENCY_EDGES: usize = 8;
/// Per-entity candidate cap for dependency ingest, so one huge entity bucket
/// cannot dominate the scan (the old code scanned the whole heap anyway).
const MAX_CANDIDATES_PER_ENTITY: usize = 64;

/// Link a fresh item to prior non-dropped items that share at least one
/// entity (new item depends on prior), bounded per item, then store it.
/// Gated by `dependency_expansion` so `baseline_v0()` records no edges.
/// Candidates come from the entity index — O(entities x bucket), not an
/// O(heap) scan — and are merged newest-first (slot order = creation order),
/// matching the previous reverse-heap behavior. Prior items reuse their
/// precomputed entity signature, so ingest never re-parses content.
pub(crate) fn push_linked(
    state: &mut State,
    config: &SimpleContextConfig,
    mut item: ContextItem,
) -> ContextItemId {
    if config.dependency_expansion {
        let entities = item.entities.clone();
        if !entities.is_empty() {
            let mut seen: HashSet<ContextItemId> = HashSet::new();
            let mut candidates: Vec<ContextItemId> = Vec::new();
            for entity in &entities {
                let mut added = 0usize;
                // The bucket is slot-ordered (creation order); scan it
                // NEWEST first so the per-entity candidate budget is spent
                // on the freshest priors. Taking the head would cap to the
                // OLDEST 64 whenever a bucket outgrows the budget
                // (PROCESS-01/CONTEXT-01 newest-first contract).
                for id in state.items.indexes().ids_for_entity(entity).iter().rev() {
                    if seen.insert(*id) {
                        candidates.push(*id);
                        added += 1;
                        if added >= MAX_CANDIDATES_PER_ENTITY {
                            break;
                        }
                    }
                }
            }
            // Newest first: slot order is creation order.
            candidates.sort_by_key(|id| state.items.indexes().get(*id).unwrap_or(0));
            candidates.reverse();

            let mut edges = 0usize;
            for id in candidates {
                let Some(index) = state.items.indexes().get(id) else {
                    continue;
                };
                let prior = &state.items[index];
                if prior.semantic.is_dead() || prior.entities.is_empty() {
                    continue;
                }
                if entities_match(&entities, &prior.entities) {
                    item.dependencies.push(DependencyEdge::shares(prior.id));
                    edges += 1;
                    if edges >= MAX_DEPENDENCY_EDGES {
                        break;
                    }
                }
            }
        }
    }
    let id = item.id;
    // The heap pushes and indexes the item in one step.
    state.items.push(item);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SimpleContextEngine;
    use agent_contracts::{ContextEngine, ContextIngress, ContextKind, ToolOutput};
    use serde_json::json;

    /// CONTEXT-01: when one entity bucket outgrows the per-entity scan
    /// budget, the budget must be spent on the NEWEST priors. The old
    /// head-truncation linked the freshest item to the oldest 64 instead.
    #[tokio::test]
    async fn dependency_candidates_are_newest_first_when_a_bucket_outgrows_the_budget() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "work on AuthService.rs".into(),
            })
            .await
            .unwrap();
        let mut ids = Vec::new();
        // 70 observations touching the same path: the bucket for
        // "AuthService.rs" outgrows the 64-candidate budget.
        for i in 0..70 {
            engine
                .ingest(ContextIngress::ToolObservation {
                    facts: None,
                    output: ToolOutput {
                        call_id: format!("call-{i}"),
                        tool_name: "fs.read".into(),
                        ok: true,
                        summary: "ok".into(),
                        model_content: format!("reading AuthService.rs pass {i}"),
                        artifact_ref: None,
                        metadata: json!({"path": "AuthService.rs"}),
                    },
                    scope_id: None,
                })
                .await
                .unwrap();
            let summaries = engine.inspect(usize::MAX).await.unwrap();
            // Inspect is slot-ordered (creation order); the new
            // observation is the last ToolObservation not yet tracked.
            ids.push(
                summaries
                    .iter()
                    .filter(|item| item.kind == ContextKind::ToolObservation)
                    .map(|item| item.id)
                    .find(|id| !ids.contains(id))
                    .expect("the new observation item"),
            );
        }

        // The 71st item links to the 8 NEWEST priors (items 62..=69), not
        // to the oldest head of the bucket.
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: ToolOutput {
                    call_id: "call-latest".into(),
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: "reading AuthService.rs final pass".into(),
                    artifact_ref: None,
                    metadata: json!({"path": "AuthService.rs"}),
                },
                scope_id: None,
            })
            .await
            .unwrap();
        let summaries = engine.inspect(usize::MAX).await.unwrap();
        let latest = summaries
            .iter()
            .rfind(|item| item.kind == ContextKind::ToolObservation)
            .expect("the latest observation");
        let expected: std::collections::HashSet<_> = ids[62..70].iter().copied().collect();
        let linked: std::collections::HashSet<_> = latest.dependencies.iter().copied().collect();
        assert_eq!(
            linked, expected,
            "the 8 edges must go to the newest priors: {linked:?} vs {expected:?}"
        );
    }

    #[tokio::test]
    async fn ingest_links_items_sharing_entities() {
        let engine = SimpleContextEngine::new(SimpleContextConfig::default());
        engine
            .ingest(ContextIngress::UserMessage {
                content: "fix AuthService.rs".into(),
            })
            .await
            .unwrap();
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: "tests passed in AuthService.rs".into(),
                    artifact_ref: None,
                    metadata: json!({"path": "AuthService.rs"}),
                },
                scope_id: None,
            })
            .await
            .unwrap();

        let summaries = engine.inspect(usize::MAX).await.unwrap();
        let user = summaries
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage)
            .expect("user message item");
        let tool = summaries
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("tool observation item");

        assert!(
            user.dependencies.is_empty(),
            "first item has no prior items"
        );
        assert!(
            tool.dependencies.contains(&user.id),
            "the tool observation must depend on the prior user message sharing its entity"
        );
    }
}
