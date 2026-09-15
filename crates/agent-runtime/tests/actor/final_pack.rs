//! T1 (R1): the final packing layer trims with ONE candidate view across
//! all droppable partitions — selected working-set bodies, foreground
//! bodies and omitable optional schemas. Optional content anywhere must go
//! before required content, and a frame whose required bodies alone exceed
//! the budget degrades honestly (typed `BudgetExcluded` required miss,
//! published request without the body) instead of failing validation or
//! pretending the body is covered.

use std::{sync::Arc, time::Duration};

use agent_contracts::{
    AgentResult, AttentionState, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemId,
    ContextItemSummary, ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger,
    ContextMaterializationMissReason, ContextQuery, ContextScope, ContextStateTransition,
    MaterializedContext, MaterializedItem, ModelRequest, RuntimeEvent, RuntimeEventEnvelope,
    SemanticState,
};
use agent_core::{CoreAuthorityConfig, PolicyApprovalGate};
use agent_runtime::{RuntimeHandle, RuntimeServices, approx_layer_tokens, spawn_runtime};

use crate::harness::{RecordingModel, TestToolDispatcher};

/// A tiny marker body stays inside the budget once the oversized optional
/// candidate is gone; the assertion reads it back off the wire request.
const REQUIRED_BODY_MARKER: &str = "REQUIRED-BODY-MARKER";
/// Under the [`agent_contracts::MAX_FOREGROUND_TOKENS`] validation cap, but
/// far above the whole input budget of the tiny-window round.
const OPTIONAL_FOREGROUND_BODY: &str = "OPTIONAL-FOREGROUND-MARKER";

fn required_body() -> MaterializedItem {
    MaterializedItem {
        item_id: ContextItemId::new(),
        kind: ContextKind::Note,
        scope: ContextScope::Task,
        attention: AttentionState::Active,
        semantic: SemanticState::Live,
        retention: agent_contracts::ContextRetention::Working,
        content: format!("{REQUIRED_BODY_MARKER}:{}", "r".repeat(400)),
        source: None,
        file_path: None,
        file_revision: None,
        file_start_line: None,
        file_end_line: None,
        partial_body: false,
    }
}

fn optional_foreground_body() -> MaterializedItem {
    MaterializedItem {
        item_id: ContextItemId::new(),
        kind: ContextKind::Note,
        scope: ContextScope::Task,
        attention: AttentionState::Active,
        semantic: SemanticState::Live,
        retention: agent_contracts::ContextRetention::Working,
        content: format!("{OPTIONAL_FOREGROUND_BODY}:{}", "f".repeat(8_096)),
        source: None,
        file_path: None,
        file_revision: None,
        file_start_line: None,
        file_end_line: None,
        partial_body: false,
    }
}

/// Engine fixture: ignores the pack budget handed to it and returns the
/// frame the test installed, so the runtime's own final packing layer is
/// what has to resolve the overshoot.
#[derive(Debug, Default)]
struct SplitFrameEngine {
    items: Vec<MaterializedItem>,
    foreground: Vec<MaterializedItem>,
}

impl SplitFrameEngine {
    fn new(items: Vec<MaterializedItem>, foreground: Vec<MaterializedItem>) -> Self {
        Self { items, foreground }
    }
}

#[async_trait::async_trait]
impl ContextEngine for SplitFrameEngine {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        let required_item_ids = self
            .items
            .iter()
            .map(|item| item.item_id)
            .collect::<Vec<_>>();
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: None,
            task: None,
            items: self.items.clone(),
            external: agent_contracts::ContextMapView::default(),
            selected: Vec::new(),
            approx_tokens: 4_000,
            foreground: self.foreground.clone(),
            required_item_ids,
            required_misses: Default::default(),
            optional_misses: Default::default(),
            diagnostics: ContextDiagnostics::default(),
        })
    }
    async fn open_scope(
        &self,
        _kind: agent_contracts::ScopeKind,
        _parent: Option<agent_contracts::ScopeId>,
    ) -> AgentResult<agent_contracts::ScopeId> {
        Ok(agent_contracts::ScopeId::new())
    }
    async fn close_scope(
        &self,
        _scope_id: agent_contracts::ScopeId,
    ) -> AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        Ok(ContextDiagnostics::default())
    }
    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        Ok(Vec::new())
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
}

/// Window 3_200, output reserve 1_200 -> a 2_000-token input budget. The
/// fixed layers alone stay well under it (a bare round measures < 1_100),
/// the required body adds ~100 tokens, and the optional foreground body
/// adds ~2_024 more: exactly one of the two bodies must go.
fn tiny_window_model() -> Arc<RecordingModel> {
    Arc::new(RecordingModel {
        context_window: 3_200,
        max_output_tokens: 1_200,
        ..RecordingModel::default()
    })
}

async fn spawn(
    model: Arc<RecordingModel>,
    engine: Arc<SplitFrameEngine>,
) -> (
    RuntimeHandle,
    tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) {
    let kernel = Arc::new(RuntimeServices::new(
        CoreAuthorityConfig::default(),
        engine,
        model,
        Arc::new(TestToolDispatcher),
        Arc::new(PolicyApprovalGate::read_only()),
        None,
    ));
    let (handle, _task) = spawn_runtime(kernel);
    let events = handle.subscribe();
    handle.start().await.unwrap();
    (handle, events)
}

/// Drains the event stream until the turn commits (or the deadline passes)
/// and returns everything observed, so failures report facts instead of
/// timing out blind.
async fn drain_turn(
    events: &mut tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> Vec<RuntimeEvent> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(envelope)) => {
                let done = matches!(envelope.event, RuntimeEvent::TurnCompleted);
                seen.push(envelope.event);
                if done {
                    return seen;
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
            // The quiet window elapsed: the turn never committed.
            Err(_) => break,
        }
    }
    seen
}

fn request_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn required_misses_of(events: &[RuntimeEvent]) -> Vec<RuntimeEvent> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::ContextDegraded { .. } | RuntimeEvent::TurnCommitFailed { .. }
            )
        })
        .cloned()
        .collect()
}

/// R1 counterexample: the selected layer holds only a small REQUIRED body
/// while the foreground layer holds a large OPTIONAL one, and the assembled
/// input is slightly over budget. Dropping the optional foreground body
/// fits; the required body must survive with no `BudgetExcluded` required
/// miss. The old partition order trimmed `items` first, sacrificed the
/// required body, and then failed the final materialization validation
/// ("required item missing from the final frame") — manufacturing a loss
/// that no honest packing needed.
#[tokio::test]
async fn final_pack_drops_optional_foreground_before_a_required_selected_body() {
    let model = tiny_window_model();
    let engine = Arc::new(SplitFrameEngine::new(
        vec![required_body()],
        vec![optional_foreground_body()],
    ));
    let (handle, mut events) = spawn(model.clone(), engine).await;
    handle.user_message("hello".into()).await.unwrap();
    let seen = drain_turn(&mut events).await;

    assert!(
        seen.iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted)),
        "the round must publish honestly instead of fencing: {:?}",
        required_misses_of(&seen)
    );
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "the turn must send exactly one request; observed events: {:?}",
        required_misses_of(&seen)
    );
    let sent = request_text(&requests[0]);
    assert!(
        sent.contains(REQUIRED_BODY_MARKER),
        "the required body must survive final packing"
    );
    assert!(
        !sent.contains(OPTIONAL_FOREGROUND_BODY),
        "the optional foreground body is what budget pressure displaces"
    );
    let fits = approx_layer_tokens(&requests[0].messages) + approx_layer_tokens(&requests[0].tools);
    assert!(
        fits <= 2_000,
        "the published request must fit the input budget (got {fits})"
    );
    for event in seen.iter() {
        if let RuntimeEvent::ContextDegraded {
            required_misses, ..
        } = event
        {
            assert_eq!(
                required_misses.total(),
                0,
                "no required body was lost, so no required miss may exist"
            );
        }
        assert!(
            !matches!(event, RuntimeEvent::TurnCommitFailed { .. }),
            "an honest packing decision must not fence the round"
        );
    }
}

/// The honesty floor: when ONLY required content exceeds the budget, the
/// drop is real and must be reported as a typed `BudgetExcluded` required
/// miss on the published degraded frame — never silently, never as fake
/// coverage. (Before the unified packing slice this scenario could not even
/// publish: the displaced id stayed in `required_item_ids` and the final
/// materialization validation fenced the round.)
#[tokio::test]
async fn final_pack_reports_a_required_miss_when_only_required_content_exceeds_the_budget() {
    let model = tiny_window_model();
    let oversized_required = MaterializedItem {
        content: format!("{REQUIRED_BODY_MARKER}:{}", "r".repeat(24_000)),
        ..required_body()
    };
    let required_id = oversized_required.item_id;
    let engine = Arc::new(SplitFrameEngine::new(vec![oversized_required], Vec::new()));
    let (handle, mut events) = spawn(model.clone(), engine).await;
    handle.user_message("hello".into()).await.unwrap();
    let seen = drain_turn(&mut events).await;

    assert!(
        seen.iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted)),
        "the degradation must publish, not fence: {:?}",
        required_misses_of(&seen)
    );
    let degraded = seen
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ContextDegraded {
                required_misses, ..
            } => Some(required_misses.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        degraded.len(),
        1,
        "exactly one degraded-frame report for the displaced required body"
    );
    assert_eq!(degraded[0].total(), 1);
    let miss = &degraded[0].as_slice()[0];
    assert_eq!(miss.identity.item_id, Some(required_id));
    assert_eq!(
        miss.reason,
        ContextMaterializationMissReason::BudgetExcluded
    );

    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "the round still sends after degrading");
    assert!(
        !request_text(&requests[0]).contains(REQUIRED_BODY_MARKER),
        "the body that could not fit is honestly absent from the request"
    );
    for event in seen.iter() {
        assert!(
            !matches!(event, RuntimeEvent::TurnCommitFailed { .. }),
            "a typed degradation is not a preparation fault"
        );
    }
}

/// S2a (continuation review 258eb4eb R2): two REQUIRED records with the
/// same file window but different IDs. The budget drops one; the survivor
/// covers the dropped one's evidence, so no `BudgetExcluded` miss is
/// recorded — and the dropped id must leave `required_item_ids` even
/// though no miss was recorded. Under the old code the id stayed (retain
/// only fired on miss) and the final materialization validation fenced
/// the round with "required item missing from the final frame".
#[tokio::test]
async fn covered_required_id_leaves_required_item_ids_without_structural_failure() {
    let model = tiny_window_model();
    // A: small, B: large — same file window, different ids.
    let mut a = required_body();
    a.file_path = Some("s2a.rs".into());
    a.file_revision = Some("r1".into());
    a.file_start_line = Some(1);
    a.file_end_line = Some(50);
    let mut b = required_body();
    b.file_path = Some("s2a.rs".into());
    b.file_revision = Some("r1".into());
    b.file_start_line = Some(1);
    b.file_end_line = Some(50);
    b.content = format!("{}:{}", "B-BODY-MARKER", "b".repeat(2_000));
    let id_a = a.item_id;
    let engine = Arc::new(SplitFrameEngine::new(vec![a, b], Vec::new()));
    let (handle, mut events) = spawn(model.clone(), engine).await;
    handle.user_message("hello".into()).await.unwrap();
    let seen = drain_turn(&mut events).await;

    assert!(
        seen.iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCompleted)),
        "the round must publish honestly instead of fencing: {:?}",
        required_misses_of(&seen)
    );
    // The wire request carries exactly one of the two records (the
    // smaller one that fits); the larger one's evidence obligation is
    // satisfied by the survivor's file-window coverage.
    let requests = model.requests.lock().unwrap();
    let sent = request_text(&requests[0]);
    assert!(
        sent.contains(&id_a.to_string()) || sent.contains("B-BODY-MARKER"),
        "one of the two same-window records must reach the wire"
    );
    // No BudgetExcluded miss: the evidence is covered, not lost.
    for event in seen.iter() {
        if let RuntimeEvent::ContextDegraded {
            required_misses, ..
        } = event
        {
            assert_eq!(
                required_misses.total(),
                0,
                "covered evidence must not produce a required miss"
            );
        }
        assert!(
            !matches!(event, RuntimeEvent::TurnCommitFailed { .. }),
            "coverage satisfied the obligation; no fencing"
        );
    }
}
