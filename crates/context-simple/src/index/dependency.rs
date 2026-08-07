use agent_contracts::{ContextItem, ContextItemId, ContextState};

use crate::engine::{SimpleContextConfig, State};
use crate::index::entity::entities_match;

/// Per-item cap on explicit dependency edges recorded at ingest.
pub(crate) const MAX_DEPENDENCY_EDGES: usize = 8;

/// Link a fresh item to prior non-dropped items that share at least one
/// entity (new item depends on prior), bounded per item, then store it.
/// Gated by `dependency_expansion` so `baseline_v0()` records no edges.
/// Prior items reuse their precomputed entity signature, so ingest does not
/// re-parse every old item's content (O(N) signature lookups, not O(N)
/// re-extractions).
pub(crate) fn push_linked(
    state: &mut State,
    config: &SimpleContextConfig,
    mut item: ContextItem,
) -> ContextItemId {
    if config.dependency_expansion {
        let entities = item.entities.clone();
        if !entities.is_empty() {
            let mut edges = 0usize;
            for prior in state.items.iter().rev() {
                if prior.state == ContextState::Dropped {
                    continue;
                }
                if prior.entities.is_empty() {
                    continue;
                }
                if entities_match(&entities, &prior.entities) {
                    item.dependencies.push(prior.id);
                    edges += 1;
                    if edges >= MAX_DEPENDENCY_EDGES {
                        break;
                    }
                }
            }
        }
    }
    let id = item.id;
    state.items.push(item);
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SimpleContextEngine;
    use agent_contracts::{ContextEngine, ContextIngress, ContextKind, ToolOutput};
    use serde_json::json;

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
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "shell.exec".into(),
                    ok: true,
                    summary: "ok".into(),
                    model_content: "tests passed in AuthService.rs".into(),
                    artifact_ref: None,
                    metadata: json!({}),
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
