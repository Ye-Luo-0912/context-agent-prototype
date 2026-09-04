//! Recovery-surface admission at the actor level: after a trusted typed
//! `parent_path_not_found` result proves the recovery contract requires
//! topology mutation, the exact host-owned `fs.mkdir` is surfaced with
//! `RecoverySurface` provenance for exactly one decision. Unrelated missing
//! reads never change the surface, and the provenance widens visibility
//! only — approval authority is unchanged.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, RuntimeEvent,
    RuntimeEventEnvelope, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOutcome, ToolOutput,
    ToolRisk, ToolSpec,
};
use agent_core::PolicyApprovalGate;
use agent_runtime::{ModuleHost, RuntimeInstance, RuntimeServices};
use serde_json::json;
use tokio::sync::Mutex;

use crate::harness::*;

/// Scripted model: call one tool per round, then finish.
#[derive(Debug)]
struct ScriptedToolModel {
    rounds: AtomicUsize,
    script: Vec<String>,
}

#[async_trait::async_trait]
impl ModelTransport for ScriptedToolModel {
    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
        let round = self.rounds.fetch_add(1, Ordering::SeqCst);
        let Some(tool) = self.script.get(round) else {
            return Ok(ModelOutput {
                content: "done".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            });
        };
        Ok(ModelOutput {
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: format!("call-{round}"),
                name: tool.clone(),
                arguments: json!({"path": "a/b/file.txt"}),
            }],
            usage: Default::default(),
        })
    }
}

/// Dispatcher that scripts a typed missing-parent refusal for `fs.write`,
/// a plain `path_not_found` for `fs.read`, and an `ok` result for
/// `fs.mkdir`. Every executed call is recorded. All scripted tools are
/// optional catalog candidates (the production `fs.mkdir` is catalog-cold,
/// not fail-closed), so the recovery requirement controls their admission.
#[derive(Debug)]
struct ScriptedRecoveryDispatcher {
    executed: Arc<Mutex<Vec<String>>>,
    fs_write_risk: ToolRisk,
}

#[async_trait::async_trait]
impl ToolDispatcher for ScriptedRecoveryDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "fs.write".into(),
                description: "scripted".into(),
                input_schema: json!({"type": "object"}),
                risk: self.fs_write_risk,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "fs.mkdir".into(),
                description: "scripted".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::WorkspaceWrite,
                output_budget: None,
                roles: Vec::new(),
            },
            ToolSpec {
                name: "fs.read".into(),
                description: "scripted".into(),
                input_schema: json!({"type": "object"}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            },
        ]
    }
    fn may_omit_from_round(&self, _name: &str) -> bool {
        true
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        self.executed.lock().await.push(request.call.name.clone());
        let output = match request.call.name.as_str() {
            "fs.write" => ToolOutput {
                call_id: request.call.id,
                tool_name: "fs.write".into(),
                ok: false,
                summary: "fs.write refused: parent_path_not_found".into(),
                model_content: "parent directory a/b does not exist".into(),
                artifact_ref: None,
                metadata: json!({
                    "failure_class": "PathNotFound",
                    "next_directory": "a/b",
                }),
            },
            "fs.read" => ToolOutput {
                call_id: request.call.id,
                tool_name: "fs.read".into(),
                ok: false,
                summary: "fs.read refused: path_not_found".into(),
                model_content: "no such file".into(),
                artifact_ref: None,
                metadata: json!({"failure_class": "PathNotFound"}),
            },
            "fs.mkdir" => ToolOutput {
                call_id: request.call.id,
                tool_name: "fs.mkdir".into(),
                ok: true,
                summary: "fs.mkdir created a/b".into(),
                model_content: "created".into(),
                artifact_ref: None,
                metadata: json!({"path": "a/b"}),
            },
            other => panic!("unexpected tool {other}"),
        };
        Ok(ToolOutcome::Value(output))
    }
}

/// Run one turn and collect every tool-surface report keyed by the model
/// round it planned (rounds are 1-based: the first decision is round 1).
/// Returns false if the turn never completed.
async fn run_turn_collecting_reports(
    instance: &RuntimeInstance,
    mut events: tokio::sync::broadcast::Receiver<RuntimeEventEnvelope>,
) -> (
    std::collections::BTreeMap<usize, agent_contracts::ToolSurfacePlanReport>,
    bool,
) {
    let handle = instance.handle();
    handle
        .set_focus("create a/b/file.txt".into())
        .await
        .unwrap();
    handle.user_message("keep going".into()).await.unwrap();
    let mut reports = std::collections::BTreeMap::new();
    let mut completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while tokio::time::Instant::now() < deadline && !completed {
        while let Ok(envelope) = events.try_recv() {
            match envelope.event {
                RuntimeEvent::ToolSurfacePlanned { report } => {
                    reports.insert(report.model_round, report);
                }
                RuntimeEvent::TurnCompleted => completed = true,
                _ => {}
            }
        }
        if !completed {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    (reports, completed)
}

fn origin_of(
    report: &agent_contracts::ToolSurfacePlanReport,
    name: &str,
) -> Option<(
    agent_contracts::ToolSurfaceOrigin,
    agent_contracts::ToolSurfaceDemand,
)> {
    report
        .selected
        .iter()
        .find(|row| row.tool_name == name)
        .map(|row| (row.origin, row.demand))
}

async fn instance_with(
    script: Vec<String>,
    gate: PolicyApprovalGate,
    dispatcher: Arc<dyn ToolDispatcher>,
    recovery_surface: bool,
) -> RuntimeInstance {
    let services = RuntimeServices::new(
        agent_core::CoreAuthorityConfig::default(),
        Arc::new(TestContextEngine),
        Arc::new(ScriptedToolModel {
            rounds: AtomicUsize::new(0),
            script,
        }),
        dispatcher,
        Arc::new(gate),
        None,
    )
    .with_recovery_surface(recovery_surface);
    let mut host = ModuleHost::new();
    host.start().await.expect("test module host starts");
    let instance = RuntimeInstance::spawn(host, services);
    instance.handle().start().await.unwrap();
    instance
}

fn scripted_dispatcher(
    fs_write_risk: ToolRisk,
) -> (Arc<ScriptedRecoveryDispatcher>, Arc<Mutex<Vec<String>>>) {
    let executed = Arc::new(Mutex::new(Vec::new()));
    let dispatcher = Arc::new(ScriptedRecoveryDispatcher {
        executed: executed.clone(),
        fs_write_risk,
    });
    (dispatcher, executed)
}

#[tokio::test]
async fn typed_missing_parent_arms_exact_recovery_surface_for_one_decision() {
    let (dispatcher, _executed) = scripted_dispatcher(ToolRisk::WorkspaceWrite);
    let instance = instance_with(
        vec!["fs.write".into(), "fs.mkdir".into()],
        PolicyApprovalGate::permissive(),
        dispatcher,
        true,
    )
    .await;
    let events = instance.handle().subscribe();
    let (reports, completed) = run_turn_collecting_reports(&instance, events).await;
    assert!(completed, "the turn must finish inside the test deadline");

    // Round 1 plans before any failure: `fs.mkdir` is only a catalog
    // candidate, never a recovery surface.
    let round1 = reports
        .get(&1)
        .expect("round 1 must plan a surface before the first decision");
    assert_eq!(
        origin_of(round1, "fs.mkdir"),
        Some((
            agent_contracts::ToolSurfaceOrigin::CatalogLoadedOptional,
            agent_contracts::ToolSurfaceDemand::PreferSurface
        )),
        "before the typed failure, fs.mkdir must be a plain catalog load"
    );

    // Round 2 plans after the typed `parent_path_not_found`: the exact
    // host-owned `fs.mkdir` is surfaced with recovery provenance.
    let round2 = reports
        .get(&2)
        .expect("round 2 must plan a surface after the failure settles");
    assert_eq!(
        origin_of(round2, "fs.mkdir"),
        Some((
            agent_contracts::ToolSurfaceOrigin::RecoverySurface,
            agent_contracts::ToolSurfaceDemand::PreferSurface
        )),
        "the exact recovery tool must carry recovery provenance for one decision"
    );

    // Round 3 plans after that decision consumed the request: the recovery
    // surface is gone. The tool may stay rooted by the independent
    // active-call result-delivery lease (the model called it last round),
    // but its provenance is no longer recovery-derived.
    let round3 = reports
        .get(&3)
        .expect("round 3 must plan a surface for the finishing decision");
    assert!(
        round3
            .selected
            .iter()
            .all(|row| row.origin != agent_contracts::ToolSurfaceOrigin::RecoverySurface),
        "the recovery surface must not outlive its single decision"
    );
    assert_ne!(
        origin_of(round3, "fs.mkdir").map(|(origin, _)| origin),
        Some(agent_contracts::ToolSurfaceOrigin::RecoverySurface),
        "fs.mkdir must lose recovery provenance after one decision"
    );
}

#[tokio::test]
async fn unrelated_missing_read_never_arms_the_recovery_surface() {
    let (dispatcher, _executed) = scripted_dispatcher(ToolRisk::WorkspaceWrite);
    let instance = instance_with(
        vec!["fs.read".into()],
        PolicyApprovalGate::read_only(),
        dispatcher,
        true,
    )
    .await;
    let events = instance.handle().subscribe();
    let (reports, completed) = run_turn_collecting_reports(&instance, events).await;
    assert!(completed, "the turn must finish inside the test deadline");

    let round2 = reports
        .get(&2)
        .expect("round 2 must plan a surface after the read settles");
    assert!(
        round2
            .selected
            .iter()
            .all(|row| row.origin != agent_contracts::ToolSurfaceOrigin::RecoverySurface),
        "an observation-style missing read carries no recovery contract, so \
         the surface must not change, got {round2:?}"
    );
    assert!(
        reports.values().all(|report| report
            .selected
            .iter()
            .all(|row| row.origin != agent_contracts::ToolSurfaceOrigin::RecoverySurface)),
        "no decision in this turn may see a recovery surface"
    );
}

#[tokio::test]
async fn recovery_surface_never_bypasses_the_approval_gate() {
    // The dispatcher scripts `fs.write` as read-only so a read-only policy
    // still executes it and its typed refusal can arm the recovery request.
    // `fs.mkdir` stays a workspace write: even after the recovery surface
    // marks it, the same policy must refuse to execute it.
    let (dispatcher, executed) = scripted_dispatcher(ToolRisk::ReadOnly);
    let instance = instance_with(
        vec!["fs.write".into(), "fs.mkdir".into()],
        PolicyApprovalGate::read_only(),
        dispatcher,
        true,
    )
    .await;
    let events = instance.handle().subscribe();
    let (reports, completed) = run_turn_collecting_reports(&instance, events).await;
    assert!(completed, "the turn must finish inside the test deadline");

    let round2 = reports
        .get(&2)
        .expect("round 2 must plan a surface after the failure settles");
    assert_eq!(
        origin_of(round2, "fs.mkdir"),
        Some((
            agent_contracts::ToolSurfaceOrigin::RecoverySurface,
            agent_contracts::ToolSurfaceDemand::PreferSurface
        )),
        "the recovery surface still surfaces the exact tool"
    );

    // Visibility widened, authority unchanged: the recovery-marked
    // workspace write must not execute under a read-only policy. Only the
    // read-only-risked `fs.write` was dispatched; `fs.mkdir` was refused
    // before dispatch.
    let executed = executed.lock().await;
    assert_eq!(
        &*executed,
        &["fs.write".to_string()],
        "the recovery surface must never bypass approval: fs.mkdir stays a \
         denied workspace write, got {executed:?}"
    );
}

#[tokio::test]
async fn switch_off_preserves_the_catalog_cold_baseline() {
    // The shipped default keeps `fs.mkdir` catalog-cold: even a typed
    // missing-parent refusal must not change the surface when the recovery
    // source is off. This is the baseline arm of the isolation paired gate.
    let (dispatcher, _executed) = scripted_dispatcher(ToolRisk::WorkspaceWrite);
    let instance = instance_with(
        vec!["fs.write".into(), "fs.mkdir".into()],
        PolicyApprovalGate::permissive(),
        dispatcher,
        false,
    )
    .await;
    let events = instance.handle().subscribe();
    let (reports, completed) = run_turn_collecting_reports(&instance, events).await;
    assert!(completed, "the turn must finish inside the test deadline");

    let round2 = reports
        .get(&2)
        .expect("round 2 must plan a surface after the failure settles");
    assert!(
        round2
            .selected
            .iter()
            .all(|row| row.origin != agent_contracts::ToolSurfaceOrigin::RecoverySurface),
        "with the switch off, the typed failure must not change the surface, \
         got {round2:?}"
    );
    assert!(
        reports.values().all(|report| report
            .selected
            .iter()
            .all(|row| row.origin != agent_contracts::ToolSurfaceOrigin::RecoverySurface)),
        "no decision in the baseline arm may see a recovery surface"
    );
}
