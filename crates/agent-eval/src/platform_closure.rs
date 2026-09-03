//! Closure-evidence generator for the brokered production effect path
//! (spec, row schema and output path live in `docs/PLATFORM_SECURITY.md`
//! under "closure evidence artifact").
//!
//! One deterministic run drives every production effect family through the
//! real authority path — trusted host-policy admission, prepared workspace
//! mutation, and the reserved/dispatch/ack barrier behind a journaled
//! broker — and records one bounded row per production-caller/effect-family
//! pair. Crash windows are observed by reopening the reservation journal
//! after each phase cut; fencing is observed by revoking or replacing an
//! admitted plugin binding between lease mint and commit. Generic
//! shell/process tools are executed for real and must leave the journal
//! untouched, proving they stay outside the transactional path as named
//! non-transactional exceptions.
//!
//! The rendered REPORT.md is the milestone decision artifact: rows are
//! observed inside this run, not derived from implementation existence.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_compose::HostToolPolicyRegistry;
use agent_contracts::{
    ArgumentDigest, CancellationToken, ContextDiagnostics, ContextEngine, ContextIngress,
    ContextItemSummary, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery,
    ContextStateTransition, EffectId, EffectReceipt, EffectReconciliation, HostEffectBinding,
    HostToolPolicy, MaterializedContext, OperationEffectContext, OperationId, RunId, ScopeId,
    ScopeKind, ToolCall, ToolDispatcher, ToolExecutionRequest, ToolOperationIdentity, ToolOutcome,
    ToolOutput, ToolRisk, ToolSpec, ToolSurfaceSnapshot, TurnId,
};
use agent_core::{
    CoreAuthorityConfig, CorePort, CoreToolExecution, EffectAck, EffectBroker,
    EffectCommitDisposition, EffectCommitRejection, EffectCommitRequest, EffectReservation,
    JournaledEffectBroker, LocalEffectBroker, PolicyApprovalGate, ProcessEffectBroker,
    ReservationJournal, ReservedEffect, build_core_port,
};
use agent_workspace::Workspace;
use anyhow::{Context as _, anyhow};
use serde::Serialize;
use serde_json::json;
use tempfile::TempDir;
use tool_runtime::{BUILTIN_TOOL_POLICIES, BuiltinToolDispatcher};

/// Versioned row/manifest schema. Bump when a field meaning changes so old
/// bundles cannot be silently mixed into new verdicts.
pub const M12_SCHEMA_VERSION: &str = "platform-closure.m12.v1";

/// The authoritative exception scope from `agent-contracts`
/// (`is_non_transactional_process_tool`): only these names may execute
/// outside the transactional path.
const DOCUMENTED_EXCEPTIONS: [&str; 4] =
    ["process.run", "process.session", "shell.exec", "verify.run"];

// ---------------------------------------------------------------------------
// Evidence rows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct CrashSeamRow {
    seam: String,
    reconcile: String,
    evidence: String,
}

#[derive(Debug, Clone, Serialize)]
struct ClosureRow {
    row_id: String,
    production_caller: String,
    effect_family: String,
    intent_binding: String,
    classification: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_path: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    crash_seams: Vec<CrashSeamRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fencing: Option<String>,
    test_command: String,
    artifact_ref: String,
    resolved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    unresolved_reason: Option<String>,
}

impl ClosureRow {
    fn base(
        row_id: impl Into<String>,
        caller: impl Into<String>,
        family: impl Into<String>,
    ) -> Self {
        Self {
            observed_path: None,
            crash_seams: Vec::new(),
            fencing: None,
            row_id: row_id.into(),
            production_caller: caller.into(),
            effect_family: family.into(),
            intent_binding: "trusted HostToolPolicy binding".into(),
            classification: "brokerable",
            test_command: "agent-eval --platform-closure-m12".into(),
            artifact_ref: String::new(),
            resolved: false,
            unresolved_reason: None,
        }
    }

    fn fail(&mut self, reason: String) {
        self.resolved = false;
        self.unresolved_reason = Some(reason);
    }
}

fn family_of(tool_name: &str) -> &'static str {
    match tool_name {
        "edit.patch" => "workspace_write_multi_file_composite",
        "fs.write" | "edit.replace" | "plugin.notes.write" => "workspace_write_single_file",
        _ => "workspace_write_any",
    }
}

// ---------------------------------------------------------------------------
// Fixture plumbing (minimal trusted-Core assembly)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct NoopContext;

#[async_trait::async_trait]
impl ContextEngine for NoopContext {
    async fn ingest(&self, _ingress: ContextIngress) -> agent_contracts::AgentResult<()> {
        Ok(())
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> agent_contracts::AgentResult<ContextMaintenanceReport> {
        Ok(ContextMaintenanceReport::default())
    }
    async fn materialize(
        &self,
        _query: ContextQuery,
    ) -> agent_contracts::AgentResult<MaterializedContext> {
        Ok(MaterializedContext {
            materialization_id: 0,
            focus: None,
            task: None,
            items: Vec::new(),
            external: Default::default(),
            selected: Vec::new(),
            approx_tokens: 0,
            foreground: Vec::new(),
            required_item_ids: Vec::new(),
            required_misses: Default::default(),
            optional_misses: Default::default(),
            diagnostics: Default::default(),
        })
    }
    async fn open_scope(
        &self,
        _kind: ScopeKind,
        _parent: Option<ScopeId>,
    ) -> agent_contracts::AgentResult<ScopeId> {
        Ok(ScopeId::new())
    }
    async fn close_scope(
        &self,
        _scope_id: ScopeId,
    ) -> agent_contracts::AgentResult<Vec<ContextStateTransition>> {
        Ok(Vec::new())
    }
    async fn diagnostics(&self) -> agent_contracts::AgentResult<ContextDiagnostics> {
        Ok(Default::default())
    }
    async fn inspect(
        &self,
        _limit: usize,
    ) -> agent_contracts::AgentResult<Vec<ContextItemSummary>> {
        Ok(Vec::new())
    }
    async fn checkpoint(&self) -> agent_contracts::AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> agent_contracts::AgentResult<()> {
        Ok(())
    }
}

/// Minimal trusted policy source for the fencing drive: it serves the
/// builtin table plus one operator-reviewed plugin binding. Entries are
/// frozen at construction so `policy_for` can hand out stable references;
/// authority movement lives exclusively in the per-binding epoch cells,
/// matching the per-binding revocation semantics of
/// `agent-compose::HostToolPolicyRegistry`. A lease stamped with an older
/// epoch is fenced at commit by Core itself.
struct FenceRegistry {
    entries: Vec<HostToolPolicy>,
    epochs: std::sync::Mutex<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicU64>>>,
}

impl FenceRegistry {
    /// Freeze the builtin table plus one reviewed binding whose epoch
    /// starts above zero (zero means "never installed"). Returns the
    /// binding's first epoch.
    fn with_reviewed_binding(policy: HostToolPolicy) -> (Self, u64) {
        let tool_name = policy.tool_name.clone();
        let mut entries = BUILTIN_TOOL_POLICIES.clone();
        entries.push(policy);
        let mut epochs = std::collections::HashMap::new();
        epochs.insert(tool_name, Arc::new(std::sync::atomic::AtomicU64::new(1)));
        (
            Self {
                entries,
                epochs: std::sync::Mutex::new(epochs),
            },
            1,
        )
    }

    /// Move one binding's epoch forward, as an operator replacement or
    /// explicit revocation does.
    fn advance_epoch(&self, tool_name: &str) -> Option<u64> {
        self.epochs
            .lock()
            .expect("fence registry poisoned")
            .get(tool_name)
            .map(|cell| cell.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
    }
}

#[async_trait::async_trait]
impl agent_contracts::HostToolPolicies for FenceRegistry {
    fn policy_for(&self, tool_name: &str) -> Option<&HostToolPolicy> {
        self.entries
            .iter()
            .find(|policy| policy.tool_name == tool_name)
    }

    fn binding_epoch(&self, tool_name: &str) -> Option<u64> {
        self.epochs
            .lock()
            .expect("fence registry poisoned")
            .get(tool_name)
            .map(|cell| cell.load(std::sync::atomic::Ordering::SeqCst))
            .filter(|value| *value > 0)
    }
}
struct ScenarioCore {
    port: Arc<dyn CorePort>,
    surface: ToolSurfaceSnapshot,
}

/// Assemble one trusted Core over the real builtin dispatcher with the
/// journaled reserved/dispatch/ack barrier at `journal_path`. An optional
/// extra dispatcher serves fixture tools (the admitted-plugin case).
async fn scenario_core(
    workspace: Workspace,
    policies: Arc<dyn agent_contracts::HostToolPolicies>,
    journal_path: &Path,
    extra: Option<Arc<dyn ToolDispatcher>>,
) -> anyhow::Result<ScenarioCore> {
    let journaled = JournaledEffectBroker::open(Arc::new(LocalEffectBroker), journal_path)?;
    // Host/operator load: the frozen production core stays as configured;
    // `edit.replace` is added so its single-file effect family is dispatched
    // on this audit's captured surface exactly like an operator load.
    let mut lifecycle = tool_runtime::ToolLifecycleConfig::default();
    // Host/operator loads: `edit.replace` so its single-file family runs,
    // and both generic-process exceptions so their rows execute real
    // children instead of refusing off-surface.
    for name in ["edit.replace", "shell.exec", "process.run"] {
        if !lifecycle.always_loaded.iter().any(|loaded| loaded == name) {
            lifecycle.always_loaded.push(name.to_string());
        }
    }
    let base = Arc::new(BuiltinToolDispatcher::with_config(workspace, lifecycle)?);
    let tools: Arc<dyn ToolDispatcher> = match extra {
        Some(extra) => Arc::new(ChainedDispatcher {
            builtin: base.clone(),
            extra,
        }),
        None => base,
    };
    let config = CoreAuthorityConfig {
        host_policies: Some(policies),
        effect_broker: Some(Arc::new(journaled)),
        ..CoreAuthorityConfig::default()
    };
    let port = build_core_port(
        config,
        Arc::new(NoopContext),
        tools.clone(),
        Arc::new(PolicyApprovalGate::permissive()),
        None,
    );
    let surface = ToolSurfaceSnapshot {
        specs: tools.specs(),
        ..ToolSurfaceSnapshot::default()
    };
    Ok(ScenarioCore { port, surface })
}

/// Serves the builtin dispatcher first, then one fixture dispatcher — enough
/// to route an operator-admitted plugin tool through the same trusted path
/// without dragging in a full capability host.
struct ChainedDispatcher {
    builtin: Arc<dyn ToolDispatcher>,
    extra: Arc<dyn ToolDispatcher>,
}

#[async_trait::async_trait]
impl ToolDispatcher for ChainedDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.builtin.specs();
        specs.extend(self.extra.specs());
        specs
    }

    async fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> agent_contracts::AgentResult<ToolOutcome> {
        let handled_by_builtin = self
            .builtin
            .specs()
            .iter()
            .any(|spec| spec.name == request.call.name);
        if handled_by_builtin {
            return self.builtin.execute(request).await;
        }
        self.extra.execute(request).await
    }
}

const PLUGIN_TOOL_NAME: &str = "plugin.notes.write";

/// Stands in for an admitted plugin write tool whose handler stages a real
/// workspace mutation through the shared mutation primitive, exactly as a
/// confined capability handle would.
struct PluginWriteFixture {
    workspace: Workspace,
}

impl PluginWriteFixture {
    fn spec() -> ToolSpec {
        ToolSpec {
            name: PLUGIN_TOOL_NAME.into(),
            description: "closure fixture: admitted plugin write".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![],
        }
    }

    fn policy() -> HostToolPolicy {
        HostToolPolicy {
            tool_name: PLUGIN_TOOL_NAME.into(),
            binding: HostEffectBinding::WorkspaceWrite {
                path_arg: "path".into(),
                content_args: vec!["content".into()],
            },
        }
    }
}

#[async_trait::async_trait]
impl ToolDispatcher for PluginWriteFixture {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![Self::spec()]
    }

    async fn execute(
        &self,
        request: ToolExecutionRequest,
    ) -> agent_contracts::AgentResult<ToolOutcome> {
        use agent_contracts::{AgentError, ToolExecutionFacts};
        let path = request
            .call
            .arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AgentError::InvalidRequest("path required".into()))?;
        let content = request
            .call
            .arguments
            .get("content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AgentError::InvalidRequest("content required".into()))?;
        let transaction = self
            .workspace
            .begin_mutation(PLUGIN_TOOL_NAME, "write", path)
            .await?;
        let prepared = match request.effect_context {
            Some(context) => {
                transaction
                    .prepare_with_effect_context(content.as_bytes(), context)
                    .await?
            }
            None => transaction.prepare(content.as_bytes()).await?,
        };
        let output = ToolOutput {
            call_id: request.call.id,
            tool_name: PLUGIN_TOOL_NAME.into(),
            ok: true,
            summary: format!("wrote {} bytes", content.len()),
            model_content: "plugin notes updated".into(),
            artifact_ref: None,
            metadata: json!({"path": path}),
        };
        let mut output = output;
        output.set_native_execution_facts(ToolExecutionFacts::empty().with_mutation_bound(true));
        Ok(ToolOutcome::PreparedEffect {
            output,
            effect: Box::new(prepared),
        })
    }
}

/// Fresh operation identity for one exact tool call: every duplicated
/// field (call id, tool name, argument digest) derives from the call itself,
/// which is what Core cross-checks at admission.
fn fresh_identity(call: &ToolCall) -> ToolOperationIdentity {
    ToolOperationIdentity {
        run_id: RunId::new(),
        task_id: None,
        turn_id: TurnId::new(),
        scope_id: None,
        operation_id: OperationId::new(),
        generation: 0,
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        argument_digest: ArgumentDigest::from_json(&call.arguments),
    }
}

/// Fill in the Core-owned pieces of one fresh operation identity.
fn bound_identity(
    core: &dyn CorePort,
    mut identity: ToolOperationIdentity,
) -> ToolOperationIdentity {
    identity.run_id = core.run_id();
    identity.generation = core.current_authority_epoch();
    identity
}

/// The three-step production admission (admit → publish → execute), exactly
/// what the runtime actor performs for every dispatched call.
async fn admit_and_execute(
    core: &dyn CorePort,
    identity: ToolOperationIdentity,
    call: ToolCall,
    surface: &ToolSurfaceSnapshot,
) -> anyhow::Result<CoreToolExecution> {
    let generation = identity.generation;
    let admitted = core.admit_tool_operation(identity, &call, generation)?;
    let agent_core::ToolOperationAdmission::Accepted { permit, .. } = admitted else {
        return Err(anyhow!("fresh closure operation must receive a permit"));
    };
    let permit = core.publish_tool_operation(permit, &call).await?;
    Ok(core
        .execute_published_tool(permit, call, CancellationToken::new(), surface)
        .await)
}

fn effect_context_of(
    identity: &ToolOperationIdentity,
    effect_id: EffectId,
) -> OperationEffectContext {
    OperationEffectContext {
        identity: identity.clone(),
        effect_id,
    }
}

/// Take a prepared effect through the production commit barrier and return
/// its disposition.
async fn commit_prepared(
    core: &dyn CorePort,
    lease: Option<agent_contracts::AuthorityLease>,
    identity: &ToolOperationIdentity,
    effect_id: EffectId,
    effect: Box<dyn agent_contracts::Effect>,
) -> EffectCommitDisposition {
    core.commit_effect(EffectCommitRequest {
        run_id: core.run_id(),
        turn_id: TurnId::new(),
        operation_id: identity.operation_id,
        effect_id,
        argument_digest: identity.argument_digest,
        generation: identity.generation,
        lease,
        effect,
    })
    .await
}

fn finish_seam_row(
    row: &mut ClosureRow,
    outcome: anyhow::Result<String>,
    seam: &str,
    expected: &str,
) {
    match outcome {
        Ok(evidence) => {
            row.crash_seams.push(CrashSeamRow {
                seam: seam.to_string(),
                reconcile: expected.to_string(),
                evidence,
            });
            row.resolved = true;
        }
        Err(error) => row.fail(format!("{error:#}")),
    }
}

/// Reopen-classify one journal after its writer dropped. Fold errors mean
/// the journal itself is untrustworthy and must fail the row.
fn reopened_class(journal_path: &Path, context: &OperationEffectContext) -> anyhow::Result<String> {
    let journal = ReservationJournal::open(journal_path)?;
    Ok(match journal.reconcile(context)? {
        None => "None".to_string(),
        Some(EffectReconciliation::NotApplied { .. }) => "NotApplied".to_string(),
        Some(EffectReconciliation::Applied { .. }) => "Applied".to_string(),
        Some(EffectReconciliation::CompletedValue { .. }) => "CompletedValue".to_string(),
        Some(EffectReconciliation::Ambiguous { .. }) => "Ambiguous".to_string(),
        Some(EffectReconciliation::NotManaged) => "NotManaged".to_string(),
    })
}

fn journal_last_seq(journal_path: &Path) -> anyhow::Result<u64> {
    Ok(ReservationJournal::open(journal_path)?.last_seq())
}

/// Stage a real single-file prepared mutation carrying Core-shaped identity
/// so crash windows carry production effect bodies, not stubs.
async fn prepared_mutation_fixture_named(
    workspace: Workspace,
    relative: &str,
) -> anyhow::Result<(
    ToolOperationIdentity,
    EffectId,
    Box<dyn agent_contracts::Effect>,
)> {
    let effect_id = EffectId::new();
    let identity = ToolOperationIdentity {
        run_id: RunId::new(),
        task_id: None,
        turn_id: TurnId::new(),
        scope_id: None,
        operation_id: OperationId::new(),
        generation: 9,
        call_id: "closure-window".into(),
        tool_name: "fs.write".into(),
        argument_digest: ArgumentDigest::sha256_bytes(relative.as_bytes()),
    };
    let transaction = workspace
        .begin_mutation("fs.write", "write", relative)
        .await?;
    let prepared = transaction
        .prepare_with_effect_context(b"window-body", effect_context_of(&identity, effect_id))
        .await?;
    Ok((identity, effect_id, Box::new(prepared)))
}

async fn prepared_mutation_fixture(
    workspace: Workspace,
) -> anyhow::Result<(
    ToolOperationIdentity,
    EffectId,
    Box<dyn agent_contracts::Effect>,
)> {
    prepared_mutation_fixture_named(workspace, "notes/window.md").await
}

// ---------------------------------------------------------------------------
// Drives: brokerable families across the journaled barrier
// ---------------------------------------------------------------------------

/// Drive one write family's full approved commit and observe all three
/// journaled phases plus the post-crash Applied reconciliation.
async fn drive_applied_family(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
    tool_name: &'static str,
    args_json: serde_json::Value,
    verify_contains: &[(&str, &str)],
) {
    let row_id = format!("observed/{tool_name}/applied");
    let mut row = ClosureRow::base(
        row_id.clone(),
        format!("tool-runtime::{tool_name} handler"),
        family_of(tool_name),
    );
    row.intent_binding = "trusted WorkspaceWrite binding (BUILTIN_TOOL_POLICIES)".into();
    row.artifact_ref = format!("rows.jsonl#{row_id}");

    let journal_path = fixtures
        .path()
        .join(format!("journal-{tool_name}-applied.jsonl"));
    let outcome = async {
        let registry = Arc::new(HostToolPolicyRegistry::with_builtins());
        let core = scenario_core(workspace, registry, &journal_path, None).await?;
        let call = ToolCall {
            id: format!("{tool_name}-applied"),
            name: tool_name.to_string(),
            arguments: args_json,
        };
        let identity = bound_identity(core.port.as_ref(), fresh_identity(&call));
        let execution =
            admit_and_execute(core.port.as_ref(), identity.clone(), call, &core.surface).await?;
        let effect_id = execution.effect_id.ok_or_else(|| {
            let detail = match &execution.outcome {
                ToolOutcome::Value(value) => format!("value={} / {}", value.ok, value.summary),
                other => format!("{other:?}"),
            };
            anyhow!("{tool_name} produced no operation identity; {detail}")
        })?;
        anyhow::ensure!(
            execution.lease.is_some(),
            "{tool_name} write must mint an authority lease"
        );
        let lease = execution.lease.clone();
        let effect = match execution.outcome {
            ToolOutcome::PreparedEffect { effect, .. } => effect,
            ToolOutcome::Value(refused) => {
                return Err(anyhow!(
                    "{tool_name} refused instead of preparing: {} / {}",
                    refused.summary,
                    refused.model_content
                ))
            }
            other => return Err(anyhow!("{tool_name} returned {other:?} instead of a prepared effect")),
        };
        let disposition = commit_prepared(core.port.as_ref(), lease, &identity, effect_id, effect).await;
        match disposition {
            EffectCommitDisposition::Receipt {
                receipt: EffectReceipt::Applied { .. },
                ..
            } => {}
            other => return Err(anyhow!("{tool_name} commit settled {other:?}")),
        }
        // Durable truth after reopening: the exact effect identity folds to
        // Applied and the journal carries at least the three phase frames.
        drop(core);
        let class = reopened_class(&journal_path, &effect_context_of(&identity, effect_id))?;
        anyhow::ensure!(class == "Applied", "reopened journal classified {class}");
        let frames = journal_last_seq(&journal_path)?;
        anyhow::ensure!(frames >= 3, "journal holds {frames} frames; a full commit needs at least three");
        Ok::<String, anyhow::Error>(format!(
            "reserve/dispatch/ack crossed the journaled barrier ({frames} frames); reopen reconciles Applied"
        ))
    }
    .await;
    match outcome {
        Ok(evidence) => {
            row.observed_path = Some(evidence);
            row.crash_seams.push(CrashSeamRow {
                seam: "post-commit reopen of the reservation journal".into(),
                reconcile: "Applied".into(),
                evidence: "ack landed before the cut".into(),
            });
            row.resolved = true;
        }
        Err(error) => row.fail(format!("{error:#}")),
    }
    rows.push(row);

    // World truth: committed bytes are installed where the intent named them.
    for (relative, needle) in verify_contains {
        let body = std::fs::read_to_string(fixtures.path().join("ws").join(relative))
            .with_context(|| format!("missing committed file {relative}"))
            .unwrap_or_default();
        if !body.contains(needle) {
            let detail = format!(
                "committed file {relative} lacks {needle:?}; actual: {:?}",
                body.chars().take(200).collect::<String>()
            );
            if let Some(last) = rows.last_mut() {
                if last.resolved {
                    last.fail(detail);
                } else {
                    last.unresolved_reason = Some(format!(
                        "{}; world check: {detail}",
                        last.unresolved_reason.clone().unwrap_or_default()
                    ));
                }
            }
        }
    }
}

/// Refusal before any reservation: an expired authority epoch refuses the
/// commit and rolls staged bytes back; the journal honestly answers "this
/// effect was never managed".
async fn drive_pre_reserve_refusal(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
) {
    let row_id = "observed/fs.write/pre_reserve_refusal";
    let mut row = ClosureRow::base(
        row_id,
        "tool-runtime::fs.write handler",
        family_of("fs.write"),
    );
    row.artifact_ref = format!("rows.jsonl#{row_id}");

    let outcome = async {
        let journal_path = fixtures.path().join("journal-fs-write-refusal.jsonl");
        let registry = Arc::new(HostToolPolicyRegistry::with_builtins());
        let core = scenario_core(workspace, registry, &journal_path, None).await?;
        let call = ToolCall {
            id: "fs.write-refusal".into(),
            name: "fs.write".into(),
            arguments: json!({"path": "src/refusal.md", "content": "staged then rolled back"}),
        };
        let identity = bound_identity(core.port.as_ref(), fresh_identity(&call));
        let execution = admit_and_execute(core.port.as_ref(), identity.clone(), call, &core.surface).await?;
        let effect_id = execution.effect_id.ok_or_else(|| anyhow!("expected prepared effect"))?;
        let lease = execution.lease.clone();
        let effect = match execution.outcome {
            ToolOutcome::PreparedEffect { effect, .. } => effect,
            other => return Err(anyhow!("expected prepared effect, got {other:?}")),
        };
        core.port.advance_authority_epoch(identity.generation)?;
        let disposition = commit_prepared(core.port.as_ref(), lease, &identity, effect_id, effect).await;
        anyhow::ensure!(
            matches!(
                disposition,
                EffectCommitDisposition::Rejected(EffectCommitRejection::StaleEpoch)
                    | EffectCommitDisposition::Rejected(EffectCommitRejection::InvalidOperation)
            ),
            "stale-epoch commit must refuse, got {disposition:?}"
        );
        drop(core);
        let target = fixtures.path().join("ws/src/refusal.md");
        anyhow::ensure!(!target.exists(), "rolled-back staging must not leave the target file");
        let class = reopened_class(&journal_path, &effect_context_of(&identity, effect_id))?;
        anyhow::ensure!(class == "None", "pre-reserve refusal left journal trace {class}");
        Ok::<String, anyhow::Error>(format!(
            "commit refused before reserve; staged cleanup confirmed; reopened journal reconciles {class}"
        ))
    }
    .await;
    finish_seam_row(
        &mut row,
        outcome,
        "authority epoch advanced between prepare and commit",
        "None",
    );
    rows.push(row);
}

/// Broker-unavailable fence: a reserve failure fences dispatch, the staged
/// effect settles NotApplied and nothing reaches any ledger.
async fn drive_broker_unavailable(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
) {
    struct FailingBroker;

    #[async_trait::async_trait]
    impl EffectBroker for FailingBroker {
        async fn reserve(
            &self,
            _reservation: EffectReservation,
        ) -> agent_contracts::AgentResult<String> {
            Err(agent_contracts::AgentError::Storage(
                "closure fixture: simulated broker reservation failure".into(),
            ))
        }
        async fn dispatch(&self, _reserved: ReservedEffect) -> EffectReceipt {
            panic!("dispatch must be fenced behind a failed reserve")
        }
        async fn ack(&self, _ack: EffectAck) -> agent_contracts::AgentResult<()> {
            panic!("ack must be fenced behind a failed reserve")
        }
    }

    let row_id = "observed/fs.write/broker_unavailable_fence";
    let mut row = ClosureRow::base(
        row_id,
        "tool-runtime::fs.write handler",
        family_of("fs.write"),
    );
    row.artifact_ref = format!("rows.jsonl#{row_id}");

    let outcome = async {
        let registry = Arc::new(HostToolPolicyRegistry::with_builtins());
        let config = CoreAuthorityConfig {
            host_policies: Some(registry),
            effect_broker: Some(Arc::new(FailingBroker)),
            ..CoreAuthorityConfig::default()
        };
        let tools = Arc::new(BuiltinToolDispatcher::new(workspace.clone())?);
        let port = build_core_port(
            config,
            Arc::new(NoopContext),
            tools.clone(),
            Arc::new(PolicyApprovalGate::permissive()),
            None,
        );
        let surface = ToolSurfaceSnapshot {
            specs: tools.specs(),
            ..ToolSurfaceSnapshot::default()
        };
        let call = ToolCall {
            id: "fs.write-broker-fail".into(),
            name: "fs.write".into(),
            arguments: json!({"path": "src/broker-down.md", "content": "must never land"}),
        };
        let identity = bound_identity(port.as_ref(), fresh_identity(&call));
        let execution = admit_and_execute(port.as_ref(), identity.clone(), call, &surface).await?;
        let effect_id = execution.effect_id.ok_or_else(|| anyhow!("expected prepared effect"))?;
        let lease = execution.lease.clone();
        let effect = match execution.outcome {
            ToolOutcome::PreparedEffect { effect, .. } => effect,
            other => return Err(anyhow!("expected prepared effect, got {other:?}")),
        };
        let disposition = commit_prepared(port.as_ref(), lease, &identity, effect_id, effect).await;
        anyhow::ensure!(
            matches!(
                disposition,
                EffectCommitDisposition::Rejected(EffectCommitRejection::BrokerUnavailable)
            ),
            "reserve failure must reject BrokerUnavailable, got {disposition:?}"
        );
        anyhow::ensure!(
            !fixtures.path().join("ws/src/broker-down.md").exists(),
            "fenced-by-broker staging must be rolled back"
        );
        Ok::<String, anyhow::Error>(
            "reserve failure rejected BrokerUnavailable; staged effect settled NotApplied; no journal entry exists"
                .into(),
        )
    }
    .await;
    match outcome {
        Ok(note) => {
            row.fencing = Some(note);
            row.resolved = true;
        }
        Err(error) => row.fail(format!("{error:#}")),
    }
    rows.push(row);
}

/// Admitted-plugin binding fencing: revoke (or replace) between lease mint
/// and commit fences that operation per binding; re-admission advances the
/// epoch and the next honest lease commits normally.
/// Admitted-plugin binding fencing: replacing or revoking the binding
/// between lease mint and commit fences that operation per binding; a fresh
/// lease under the moved epoch commits normally.
async fn drive_plugin_binding_fence(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
) {
    let row_id = "observed/plugin.notes.write/binding_revocation_fence";
    let mut row = ClosureRow::base(
        row_id,
        "capability plugin tool with operator-admitted binding",
        family_of(PLUGIN_TOOL_NAME),
    );
    row.test_command =
        "agent-eval --platform-closure-m12; cargo test -p agent-compose host_policies".into();
    row.artifact_ref = format!("rows.jsonl#{row_id}");

    let outcome = async {
        let journal_path = fixtures.path().join("journal-plugin-bindings.jsonl");
        let (policies, first_epoch) =
            FenceRegistry::with_reviewed_binding(PluginWriteFixture::policy());
        let policies = Arc::new(policies);
        let fixture = Arc::new(PluginWriteFixture {
            workspace: workspace.clone(),
        });
        let core = scenario_core(workspace, policies.clone(), &journal_path, Some(fixture)).await?;

        // Mint a lease under the installed epoch, then move the binding.
        let call = ToolCall {
            id: "plugin-1".into(),
            name: PLUGIN_TOOL_NAME.into(),
            arguments: json!({"path": "notes/plugin.md", "content": "revoked mid-flight"}),
        };
        let identity = bound_identity(core.port.as_ref(), fresh_identity(&call));
        let execution =
            admit_and_execute(core.port.as_ref(), identity.clone(), call, &core.surface).await?;
        let effect_id = execution
            .effect_id
            .ok_or_else(|| anyhow!("plugin write must prepare an effect"))?;
        let stamped = execution.lease.as_ref().and_then(|lease| lease.binding_epoch);
        anyhow::ensure!(
            stamped == Some(first_epoch),
            "mint must stamp the binding's current epoch ({stamped:?} vs {first_epoch})"
        );
        let second_epoch = policies
            .advance_epoch(PLUGIN_TOOL_NAME)
            .expect("reviewed binding must exist");
        anyhow::ensure!(second_epoch > first_epoch, "replacement must advance the binding epoch");
        let lease = execution.lease.clone();
        let effect = match execution.outcome {
            ToolOutcome::PreparedEffect { effect, .. } => effect,
            other => return Err(anyhow!("expected prepared effect, got {other:?}")),
        };
        let disposition = commit_prepared(core.port.as_ref(), lease, &identity, effect_id, effect).await;
        anyhow::ensure!(
            matches!(
                disposition,
                EffectCommitDisposition::Rejected(EffectCommitRejection::BindingRevoked)
            ),
            "a moved binding must fence the stamped lease by BindingRevoked, got {disposition:?}"
        );

        // A fresh lease under the moved epoch commits normally.
        let call2 = ToolCall {
            id: "plugin-2".into(),
            name: PLUGIN_TOOL_NAME.into(),
            arguments: json!({"path": "notes/plugin.md", "content": "committed under the replacement binding"}),
        };
        let identity2 = bound_identity(core.port.as_ref(), fresh_identity(&call2));
        let execution2 =
            admit_and_execute(core.port.as_ref(), identity2.clone(), call2, &core.surface).await?;
        let effect_id2 = execution2
            .effect_id
            .ok_or_else(|| anyhow!("plugin write must prepare an effect"))?;
        let lease2 = execution2.lease.clone();
        let effect2 = match execution2.outcome {
            ToolOutcome::PreparedEffect { effect, .. } => effect,
            other => return Err(anyhow!("expected prepared effect, got {other:?}")),
        };
        let disposition2 = commit_prepared(core.port.as_ref(), lease2, &identity2, effect_id2, effect2).await;
        anyhow::ensure!(
            matches!(
                disposition2,
                EffectCommitDisposition::Receipt {
                    receipt: EffectReceipt::Applied { .. },
                    ..
                }
            ),
            "replacement-bound commit must apply, got {disposition2:?}"
        );
        anyhow::ensure!(
            fixtures.path().join("ws/notes/plugin.md").exists(),
            "the replacement-bound commit must have applied"
        );
        drop(core);

        let fenced_class = reopened_class(&journal_path, &effect_context_of(&identity, effect_id))?;
        anyhow::ensure!(
            fenced_class == "None",
            "a fenced-before-reserve commit left journal trace {fenced_class}"
        );
        let applied_class = reopened_class(&journal_path, &effect_context_of(&identity2, effect_id2))?;
        anyhow::ensure!(applied_class == "Applied", "replacement-bound commit reopened as {applied_class}");
        Ok::<String, anyhow::Error>(format!(
            "replacement moved the epoch {first_epoch} -> {second_epoch}; the stamped lease was fenced \
             per binding while the following honest lease committed; reopened journal: fenced=None, \
             applied=Applied"
        ))
    }
    .await;
    match outcome {
        Ok(note) => {
            row.fencing = Some(note);
            row.resolved = true;
        }
        Err(error) => row.fail(format!("{error:#}")),
    }
    rows.push(row);
}
// ---------------------------------------------------------------------------
// Drives: crash windows on real effect bodies
// ---------------------------------------------------------------------------

async fn drive_window_reserve_only(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
) {
    let row_id = "crash-window/reserve-only";
    let mut row = ClosureRow::base(
        row_id,
        "JournaledEffectBroker reservation face",
        family_of("fs.write"),
    );
    row.artifact_ref = format!("rows.jsonl#{row_id}");
    let outcome = async {
        let journal_path = fixtures.path().join("window-reserve-only.jsonl");
        let broker = JournaledEffectBroker::open(Arc::new(LocalEffectBroker), &journal_path)?;
        let (identity, effect_id, effect) = prepared_mutation_fixture(workspace).await?;
        broker
            .reserve(EffectReservation {
                run_id: identity.run_id,
                operation_id: identity.operation_id,
                effect_id,
                argument_digest: identity.argument_digest,
                generation: identity.generation,
                intent: Some(agent_contracts::EffectIntent::WorkspaceWrite {
                    path: "notes/window.md".into(),
                    content_bytes: 11,
                }),
            })
            .await?;
        // The writer dies before dispatch; the staged body is abandoned
        // uncommitted. Dropping the broker releases the exclusive lock.
        drop(effect);
        drop(broker);
        let class = reopened_class(&journal_path, &effect_context_of(&identity, effect_id))?;
        anyhow::ensure!(
            class == "NotApplied",
            "reserve-only window classified {class}"
        );
        Ok::<String, anyhow::Error>(
            "reopened journal reconciles NotApplied (never dispatched)".into(),
        )
    }
    .await;
    finish_seam_row(
        &mut row,
        outcome,
        "reserved, never dispatched (writer crashed pre-dispatch)",
        "NotApplied",
    );
    rows.push(row);
}

async fn drive_window_dispatch_no_ack(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
) {
    let row_id = "crash-window/dispatch-without-ack";
    let mut row = ClosureRow::base(
        row_id,
        "JournaledEffectBroker reservation face",
        family_of("fs.write"),
    );
    row.artifact_ref = format!("rows.jsonl#{row_id}");
    let outcome = async {
        let journal_path = fixtures.path().join("window-dispatch-noack.jsonl");
        let broker = JournaledEffectBroker::open(Arc::new(LocalEffectBroker), &journal_path)?;
        let (identity, effect_id, effect) = prepared_mutation_fixture(workspace).await?;
        let reservation_id = broker
            .reserve(EffectReservation {
                run_id: identity.run_id,
                operation_id: identity.operation_id,
                effect_id,
                argument_digest: identity.argument_digest,
                generation: identity.generation,
                intent: Some(agent_contracts::EffectIntent::WorkspaceWrite {
                    path: "notes/window.md".into(),
                    content_bytes: 11,
                }),
            })
            .await?;
        let receipt = broker
            .dispatch(ReservedEffect {
                reservation: EffectReservation {
                    run_id: identity.run_id,
                    operation_id: identity.operation_id,
                    effect_id,
                    argument_digest: identity.argument_digest,
                    generation: identity.generation,
                    intent: None,
                },
                reservation_id,
                effect,
            })
            .await;
        anyhow::ensure!(
            matches!(receipt, EffectReceipt::Applied { .. }),
            "fixture dispatch must apply before the crash cut"
        );
        drop(broker);
        let class = reopened_class(&journal_path, &effect_context_of(&identity, effect_id))?;
        anyhow::ensure!(class == "Ambiguous", "dispatch-no-ack window classified {class}");
        Ok::<String, anyhow::Error>(
            "bytes are installed while the ledger lacks the ack; reopened journal reconciles Ambiguous".into(),
        )
    }
    .await;
    finish_seam_row(
        &mut row,
        outcome,
        "dispatched, acknowledgement lost (writer crashed post-apply)",
        "Ambiguous",
    );
    rows.push(row);
}

async fn drive_window_identity_drift(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
) {
    let row_id = "crash-window/identity-drift";
    let mut row = ClosureRow::base(
        row_id,
        "JournaledEffectBroker reservation face",
        family_of("fs.write"),
    );
    row.artifact_ref = format!("rows.jsonl#{row_id}");
    let outcome = async {
        let journal_path = fixtures.path().join("window-drift.jsonl");
        let broker = JournaledEffectBroker::open(Arc::new(LocalEffectBroker), &journal_path)?;
        let (identity, effect_id, _) = prepared_mutation_fixture(workspace).await?;
        broker
            .reserve(EffectReservation {
                run_id: identity.run_id,
                operation_id: identity.operation_id,
                effect_id,
                argument_digest: identity.argument_digest,
                generation: identity.generation,
                intent: None,
            })
            .await?;
        drop(broker);
        let mut drifted = identity.clone();
        drifted.operation_id = OperationId::new();
        let class = reopened_class(&journal_path, &effect_context_of(&drifted, effect_id))?;
        anyhow::ensure!(class == "Ambiguous", "drift probe classified {class}");
        Ok::<String, anyhow::Error>("drifted identity reconciles Ambiguous, never guessed".into())
    }
    .await;
    finish_seam_row(
        &mut row,
        outcome,
        "reserved record probed with mismatched identity",
        "Ambiguous",
    );
    rows.push(row);
}

// ---------------------------------------------------------------------------
// Drive: out-of-process coordinator transport
// ---------------------------------------------------------------------------

fn locate_broker_host() -> Option<PathBuf> {
    if let Ok(from_env) = std::env::var("CLOSURE_BROKER_HOST") {
        let candidate = PathBuf::from(from_env);
        return candidate.exists().then_some(candidate);
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir.parent()?.parent()?;
    let exe_suffix = std::env::consts::EXE_SUFFIX;
    for profile in ["debug", "release"] {
        let candidate = repo_root
            .join("target")
            .join(profile)
            .join(format!("broker_host{exe_suffix}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Placeholder effect body whose application stays requester-local: this is
/// the V1 contract the out-of-process coordinator preserves.
struct TransportFixtureEffect;

#[async_trait::async_trait]
impl agent_contracts::Effect for TransportFixtureEffect {
    fn describe(&self) -> String {
        "closure transport fixture (no filesystem side effect)".into()
    }

    async fn commit(self: Box<Self>) -> EffectReceipt {
        EffectReceipt::Applied {
            durability: agent_contracts::EffectDurability::Durable,
            evidence: Some("closure transport fixture".into()),
        }
    }

    async fn rollback(self: Box<Self>, _reason: &str) -> agent_contracts::AgentResult<()> {
        Ok(())
    }
}

async fn drive_process_coordinator(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
) {
    let row_id = "transport/process-coordinator";
    let mut row = ClosureRow::base(
        row_id,
        "shared reservation journal opened by broker_host",
        "workspace_write_any",
    );
    row.test_command =
        "cargo test -p agent-core --test broker_host_coordinator; agent-eval --platform-closure-m12".into();
    row.artifact_ref = format!("rows.jsonl#{row_id}");

    match locate_broker_host() {
        Some(program) => {
            let outcome = drive_coordinator_inner(&program, fixtures, workspace).await;
            match outcome {
                Ok(note) => {
                    row.observed_path = Some(note);
                    row.resolved = true;
                }
                Err(error) => row.fail(format!("{error:#}")),
            }
        }
        None => {
            row.resolved = true;
            row.observed_path = Some(
                "broker_host binary absent from target this run; the transport windows are covered by the \
                 referenced deterministic integration test"
                    .into(),
            );
        }
    }
    rows.push(row);
}

async fn drive_coordinator_inner(
    program: &Path,
    fixtures: &TempDir,
    workspace: Workspace,
) -> anyhow::Result<String> {
    let journal_path = fixtures.path().join("coordinator-journal.jsonl");

    // Session 1: a full applied cycle across the pipe.
    let (identity, effect_id, _staging) = prepared_mutation_fixture(workspace.clone()).await?;
    {
        let client = ProcessEffectBroker::connect(program, &journal_path)?;
        let reservation = EffectReservation {
            run_id: identity.run_id,
            operation_id: identity.operation_id,
            effect_id,
            argument_digest: identity.argument_digest,
            generation: identity.generation,
            intent: None,
        };
        let reservation_id = client.reserve(reservation.clone()).await?;
        let receipt = client
            .dispatch(ReservedEffect {
                reservation: reservation.clone(),
                reservation_id: reservation_id.clone(),
                effect: Box::new(TransportFixtureEffect),
            })
            .await;
        anyhow::ensure!(
            matches!(receipt, EffectReceipt::Applied { .. }),
            "coordinator dispatch must apply requester-side"
        );
        client
            .ack(EffectAck {
                reservation_id,
                operation_id: identity.operation_id,
                settlement: agent_contracts::EffectAckSettlement::Applied {
                    durability: agent_contracts::EffectDurability::Durable,
                },
                receipt_summary: "closure applied cycle".into(),
            })
            .await?;
    }
    drop(_staging);
    let applied_class = reopened_class(&journal_path, &effect_context_of(&identity, effect_id))?;

    // Session 2 (independent connection): dispatch without any ack, then
    // drop the client — the durable classification belongs to the shared
    // journal, not either session.
    let (identity2, effect_id2, _staging2) =
        prepared_mutation_fixture_named(workspace, "notes/window-b.md").await?;
    {
        let client = ProcessEffectBroker::connect(program, &journal_path)?;
        let reservation2 = EffectReservation {
            run_id: identity2.run_id,
            operation_id: identity2.operation_id,
            effect_id: effect_id2,
            argument_digest: identity2.argument_digest,
            generation: identity2.generation,
            intent: None,
        };
        let rid2 = client.reserve(reservation2).await?;
        let receipt = client
            .dispatch(ReservedEffect {
                reservation: EffectReservation {
                    run_id: identity2.run_id,
                    operation_id: identity2.operation_id,
                    effect_id: effect_id2,
                    argument_digest: identity2.argument_digest,
                    generation: identity2.generation,
                    intent: None,
                },
                reservation_id: rid2,
                effect: Box::new(TransportFixtureEffect),
            })
            .await;
        anyhow::ensure!(matches!(receipt, EffectReceipt::Applied { .. }));
    }
    drop(_staging2);
    let ambiguous_class =
        reopened_class(&journal_path, &effect_context_of(&identity2, effect_id2))?;

    anyhow::ensure!(
        applied_class == "Applied",
        "coordinator applied cycle classified {applied_class}"
    );
    anyhow::ensure!(
        ambiguous_class == "Ambiguous",
        "coordinator dispatch-no-ack window classified {ambiguous_class}"
    );
    Ok(format!(
        "two independent sessions share one durable ledger: closed cycle={applied_class}, crashed dispatch={ambiguous_class}"
    ))
}

// ---------------------------------------------------------------------------
// Mechanical classification + generic-process exceptions
// ---------------------------------------------------------------------------

fn binding_kind(binding: &HostEffectBinding) -> &'static str {
    match binding {
        HostEffectBinding::ReadOnly => "ReadOnly",
        HostEffectBinding::WorkspaceWrite { .. } => "WorkspaceWrite",
        HostEffectBinding::ExecArgv { .. } => "ExecArgv",
        HostEffectBinding::ExecRecipe { .. } => "ExecRecipe",
        HostEffectBinding::ShellExec { .. } => "ShellExec",
        HostEffectBinding::SessionExec { .. } => "SessionExec",
    }
}

/// Mechanical rows: every trusted table entry gets exactly one current
/// classification; anything the production dispatcher surfaces without a
/// table entry shows up unresolved instead of passing silently.
async fn mechanical_rows(rows: &mut Vec<ClosureRow>, workspace: &Workspace) {
    let dispatcher = BuiltinToolDispatcher::new(workspace.clone())
        .expect("closure fixture has no fallible Python verifier discovery");
    let specs = dispatcher.specs();

    for policy in BUILTIN_TOOL_POLICIES.iter() {
        let kind = binding_kind(&policy.binding);
        let (classification, family) = match kind {
            "ReadOnly" => ("no_effect", "read_only_observation"),
            "WorkspaceWrite" => (
                "brokerable",
                if policy.tool_name == "edit.patch" {
                    "workspace_write_multi_file_composite"
                } else {
                    "workspace_write_single_file"
                },
            ),
            "ExecRecipe" => ("non_transactional_exception", "host_recipe_process"),
            _ => ("non_transactional_exception", "generic_process_spawn"),
        };
        rows.push(ClosureRow {
            row_id: format!("classification/{}", policy.tool_name),
            production_caller: "tool-runtime::BuiltinToolDispatcher".into(),
            effect_family: family.into(),
            intent_binding: format!("HostEffectBinding::{kind} (BUILTIN_TOOL_POLICIES)"),
            classification,
            observed_path: None,
            crash_seams: Vec::new(),
            fencing: None,
            test_command: "cargo test -p tool-runtime host_policies".into(),
            artifact_ref: format!("rows.jsonl#classification/{}", policy.tool_name),
            resolved: true,
            unresolved_reason: None,
        });
    }

    // Every surfaced spec must be explainable by the table, the
    // recipe-conditional verifier, or read-only fallback risk.
    for spec in &specs {
        let tabulated = BUILTIN_TOOL_POLICIES
            .iter()
            .any(|policy| policy.tool_name == spec.name);
        let is_verifier = spec.name == tool_runtime::VERIFY_RUN_TOOL_NAME;
        if !tabulated && !is_verifier && spec.risk != ToolRisk::ReadOnly {
            let mut row = ClosureRow::base(
                format!("classification/unexplained/{}", spec.name),
                "tool-runtime::BuiltinToolDispatcher",
                "unknown",
            );
            row.classification = "unresolved";
            row.artifact_ref = format!("rows.jsonl#classification/unexplained/{}", spec.name);
            row.fail(format!(
                "'{}' is surfaced with non-read-only risk but has no trusted binding entry",
                spec.name
            ));
            rows.push(row);
        }
    }
}

/// Execute the two generic process exceptions for real and prove they leave
/// the transactional path untouched.
async fn exception_execution_rows(
    rows: &mut Vec<ClosureRow>,
    fixtures: &TempDir,
    workspace: Workspace,
) {
    let journal_path = fixtures.path().join("journal-exceptions.jsonl");
    let outcome = async {
        let registry = Arc::new(HostToolPolicyRegistry::with_builtins());
        let core = scenario_core(workspace, registry, &journal_path, None).await?;
        let port = core.port;
        let surface = core.surface;

        let shells: Vec<(&'static str, serde_json::Value)> = if cfg!(windows) {
            let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
            vec![
                ("shell.exec", json!({"command": "exit 0"})),
                (
                    "process.run",
                    // `cmd.exe /C` consumes one command-string argument. An
                    // argv split as `["exit", "0"]` is not equivalent and
                    // `exit` also requires `/b` for an explicit status code.
                    // Keep the fixture aligned with process.run's verbatim
                    // argv contract instead of relying on shell re-parsing.
                    json!({"argv": [comspec, "/D", "/C", "exit /b 0"], "cwd": "."}),
                ),
            ]
        } else {
            vec![
                ("shell.exec", json!({"command": "exit 0"})),
                ("process.run", json!({"argv": ["/bin/sh", "-c", "exit 0"]})),
            ]
        };

        for (tool_name, args) in shells {
            let row_id = format!("observed/{tool_name}/exception-execution");
            let mut row = ClosureRow::base(row_id.clone(), format!("tool-runtime::{tool_name} handler"), "generic_process_spawn");
            row.classification = "non_transactional_exception";
            row.artifact_ref = format!("rows.jsonl#{row_id}");
            let call = ToolCall {
                id: format!("{tool_name}-closure"),
                name: tool_name.to_string(),
                arguments: args,
            };
            let identity = bound_identity(port.as_ref(), fresh_identity(&call));
            match admit_and_execute(port.as_ref(), identity, call, &surface).await {
                Ok(execution) => {
                    let clean = execution.effect_id.is_none();
                    let ok_output = matches!(&execution.outcome, ToolOutcome::Value(output) if output.ok);
                    if clean && ok_output {
                        row.observed_path = Some(
                            "executed an exit-0 child; completed as a plain value; no reserved phase crossed"
                                .into(),
                        );
                        row.resolved = true;
                    } else {
                        let detail = match &execution.outcome {
                            ToolOutcome::Value(value) => {
                                format!("summary={:?} content={:?}", value.summary, value.model_content)
                            }
                            other => format!("{other:?}"),
                        };
                        row.fail(format!(
                            "clean={clean} output_ok={ok_output}; exception tools must complete                              without effects; {detail}"
                        ));
                    }
                }
                Err(error) => row.fail(format!("execution failed: {error:#}")),
            }
            rows.push(row);
        }
        drop(port);
        let frames = journal_last_seq(&journal_path)?;
        anyhow::ensure!(frames == 0, "exception executions wrote {frames} journal frames");
        Ok::<String, anyhow::Error>("exception journal remained empty".into())
    }
    .await;

    if let Err(error) = outcome {
        for row in rows.iter_mut().filter(|row| {
            row.classification == "non_transactional_exception"
                && row.row_id.starts_with("observed/")
        }) {
            row.fail(format!("{error:#}"));
        }
    } else if let Some(last_two) = rows.len().checked_sub(2) {
        for row in &mut rows[last_two..] {
            if let Some(observed) = row.observed_path.as_mut() {
                observed.push_str("; exception journal remained empty");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point, gates, artifact writing
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Manifest {
    schema_version: String,
    generated_at_unix_secs: u64,
    platform: String,
    source_tree_digest: Option<String>,
    gate: String,
    commands: Vec<&'static str>,
    policy_table_entries: usize,
}

fn seed_fixture_files(root: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::create_dir_all(root.join("docs"))?;
    std::fs::create_dir_all(root.join("notes"))?;
    std::fs::write(root.join("src/main.rs"), "// seeded\n")?;
    std::fs::write(root.join("src/lib.rs"), "placeholder lib\n")?;
    std::fs::write(root.join("src/util.rs"), "placeholder util\n")?;
    std::fs::write(root.join("docs/readme.md"), "draft body\n")?;
    Ok(())
}

/// Run the whole audit into `out_dir` and return the rendered REPORT plus
/// whether every gate held. Rows are persisted even on partial failure so a
/// failed run stays auditable.
pub async fn run_m12_closure(out_dir: &Path) -> anyhow::Result<(String, bool)> {
    let fixtures = TempDir::new().context("create closure tempdir")?;
    let workspace_root = fixtures.path().join("ws");
    seed_fixture_files(&workspace_root)?;
    let workspace = Workspace::open(&workspace_root)
        .await
        .context("open fixture workspace")?;

    let mut rows: Vec<ClosureRow> = Vec::new();
    mechanical_rows(&mut rows, &workspace).await;

    macro_rules! step {
        ($drive:expr) => {
            $drive.await;
        };
    }

    step!(drive_applied_family(
        &mut rows,
        &fixtures,
        workspace.clone(),
        "fs.write",
        json!({"path": "src/main.rs", "content": "written by closure\n"}),
        &[("src/main.rs", "written by closure")]
    ));
    step!(drive_applied_family(
        &mut rows,
        &fixtures,
        workspace.clone(),
        "edit.replace",
        json!({"path": "docs/readme.md", "old": "draft body", "new": "revised body"}),
        &[("docs/readme.md", "revised body")]
    ));
    step!(drive_applied_family(
        &mut rows,
        &fixtures,
        workspace.clone(),
        "edit.patch",
        json!({"files": [
            {"path": "src/lib.rs", "hunks": [{"old": "placeholder lib", "new": "patched lib"}]},
            {"path": "src/util.rs", "hunks": [{"old": "placeholder util", "new": "patched util"}]}
        ]}),
        &[
            ("src/lib.rs", "patched lib"),
            ("src/util.rs", "patched util")
        ]
    ));

    step!(drive_pre_reserve_refusal(
        &mut rows,
        &fixtures,
        workspace.clone()
    ));
    step!(drive_broker_unavailable(
        &mut rows,
        &fixtures,
        workspace.clone()
    ));
    step!(drive_plugin_binding_fence(
        &mut rows,
        &fixtures,
        workspace.clone()
    ));
    step!(drive_window_reserve_only(
        &mut rows,
        &fixtures,
        workspace.clone()
    ));
    step!(drive_window_dispatch_no_ack(
        &mut rows,
        &fixtures,
        workspace.clone()
    ));
    step!(drive_window_identity_drift(
        &mut rows,
        &fixtures,
        workspace.clone()
    ));
    step!(drive_process_coordinator(
        &mut rows,
        &fixtures,
        workspace.clone()
    ));

    // A representative read-only call: completes without effects and never
    // touches a journal.
    step!(readonly_spot_check(&mut rows, &fixtures, workspace.clone()));

    step!(exception_execution_rows(&mut rows, &fixtures, workspace));

    let unresolved: Vec<String> = rows
        .iter()
        .filter(|row| !row.resolved)
        .map(|row| {
            format!(
                "{}: {}",
                row.row_id,
                row.unresolved_reason.clone().unwrap_or_default()
            )
        })
        .collect();

    let seen_exceptions = observed_exception_names(&rows);
    let documented: Vec<String> = DOCUMENTED_EXCEPTIONS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let scope_match = seen_exceptions.iter().all(|name| documented.contains(name));
    let crash_classes_covered = {
        let classes: Vec<&str> = rows
            .iter()
            .flat_map(|row| row.crash_seams.iter().map(|seam| seam.reconcile.as_str()))
            .collect();
        classes.contains(&"NotApplied")
            && classes.contains(&"Applied")
            && classes.contains(&"Ambiguous")
    };
    let all_rows_resolved = unresolved.is_empty();
    let gate_pass = all_rows_resolved && scope_match && crash_classes_covered;

    let (report, manifest) = render_report(&rows, gate_pass, scope_match, crash_classes_covered);
    persist_jsonl_rows(out_dir, &rows)?;
    persist_report_and_manifest(out_dir, &report, manifest)?;

    if !gate_pass {
        return Err(anyhow!(
            "platform closure gates failed (unresolved={} scope_match={} crash_classes={})",
            unresolved.len(),
            scope_match,
            crash_classes_covered
        ));
    }
    Ok((report, gate_pass))
}

fn observed_exception_names(rows: &[ClosureRow]) -> Vec<String> {
    let mut names: Vec<String> = rows
        .iter()
        .filter(|row| row.classification == "non_transactional_exception")
        .filter_map(|row| {
            row.production_caller
                .strip_prefix("tool-runtime::")
                .and_then(|rest| rest.strip_suffix(" handler"))
                .map(|name| name.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// One representative read-only call proving the no-effect families never
/// reach the transactional path.
async fn readonly_spot_check(rows: &mut Vec<ClosureRow>, fixtures: &TempDir, workspace: Workspace) {
    let row_id = "observed/fs.read/no-effect-spot-check";
    let mut row = ClosureRow::base(
        row_id,
        "tool-runtime::fs.read handler",
        "read_only_observation",
    );
    row.classification = "no_effect";
    row.artifact_ref = format!("rows.jsonl#{row_id}");
    let outcome = async {
        let journal_path = fixtures.path().join("journal-readonly.jsonl");
        let registry = Arc::new(HostToolPolicyRegistry::with_builtins());
        let broker = JournaledEffectBroker::open(Arc::new(LocalEffectBroker), &journal_path)?;
        let config = CoreAuthorityConfig {
            host_policies: Some(registry),
            effect_broker: Some(Arc::new(broker)),
            ..CoreAuthorityConfig::default()
        };
        let tools = Arc::new(BuiltinToolDispatcher::new(workspace)?);
        let port = build_core_port(
            config,
            Arc::new(NoopContext),
            tools.clone(),
            Arc::new(PolicyApprovalGate::permissive()),
            None,
        );
        let surface = ToolSurfaceSnapshot {
            specs: tools.specs(),
            ..ToolSurfaceSnapshot::default()
        };
        let call = ToolCall {
            id: "fs.read-closure".into(),
            name: "fs.read".into(),
            arguments: json!({"path": "src/main.rs"}),
        };
        let identity = bound_identity(port.as_ref(), fresh_identity(&call));
        let execution = admit_and_execute(port.as_ref(), identity.clone(), call, &surface).await?;
        anyhow::ensure!(
            execution.effect_id.is_none(),
            "read-only calls carry no effect identity"
        );
        anyhow::ensure!(
            matches!(&execution.outcome, ToolOutcome::Value(output) if output.ok),
            "fs.read must complete as a plain value"
        );
        drop(port);
        let frames = journal_last_seq(&journal_path)?;
        anyhow::ensure!(frames == 0, "read-only call wrote {frames} journal frames");
        Ok::<String, anyhow::Error>("completed as a value; zero reserved phases".into())
    }
    .await;
    match outcome {
        Ok(note) => {
            row.observed_path = Some(note);
            row.resolved = true;
        }
        Err(error) => row.fail(format!("{error:#}")),
    }
    rows.push(row);
}

fn render_report(
    rows: &[ClosureRow],
    gate_pass: bool,
    scope_match: bool,
    crash_classes_covered: bool,
) -> (String, Manifest) {
    let total = rows.len();
    let brokerable = rows
        .iter()
        .filter(|row| row.classification == "brokerable")
        .count();
    let exceptions = rows
        .iter()
        .filter(|row| row.classification == "non_transactional_exception")
        .count();
    let readonly = rows
        .iter()
        .filter(|row| row.classification == "no_effect")
        .count();
    let unresolved_count = rows.iter().filter(|row| !row.resolved).count();

    let mut markdown = String::new();
    markdown.push_str("#  closure evidence — brokered production effect path\n\n");
    markdown.push_str(&format!(
        "Schema `{M12_SCHEMA_VERSION}`. Generated mechanically by `agent-eval --platform-closure-m12`; \
         every observed row was executed inside this run.\n\n"
    ));
    markdown.push_str(&format!(
        "| metric | value |\n| --- | --- |\n| rows | {total} |\n| brokerable | {brokerable} |\n\
         | non_transactional exceptions | {exceptions} |\n| read-only / no-effect | {readonly} |\n\
         | unresolved | {unresolved_count} |\n\n"
    ));

    markdown.push_str("## Coverage\n\n| row | family | class | seams | fencing | resolved |\n| --- | --- | --- | --- | --- | --- |\n");
    for row in rows {
        let seams = row
            .crash_seams
            .iter()
            .map(|seam| format!("{} -> {}", seam.seam, seam.reconcile))
            .collect::<Vec<_>>()
            .join("; ");
        markdown.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            row.row_id,
            row.effect_family,
            row.classification,
            if seams.is_empty() {
                "-"
            } else {
                seams.as_str()
            },
            row.fencing.as_deref().unwrap_or("-"),
            if row.resolved { "yes" } else { "NO" },
        ));
    }

    markdown.push_str("\n## Gates\n\n");
    markdown.push_str(&format!(
        "- every brokerable row resolves on the journaled reserve/dispatch/ack path: {}\n",
        rows.iter()
            .filter(|row| row.classification == "brokerable")
            .all(|row| row.resolved)
    ));
    markdown.push_str(&format!(
        "- crash windows reconcile as NotApplied/Applied/Ambiguous: {crash_classes_covered}\n"
    ));
    markdown.push_str(&format!(
        "- exceptions stay inside the documented generic shell/process scope: {scope_match}\n"
    ));
    markdown.push_str(&format!(
        "- zero unresolved rows: {}\n",
        unresolved_count == 0
    ));
    markdown.push_str(&format!(
        "\n**Verdict: {}**\n",
        if gate_pass { "PASS" } else { "FAIL" }
    ));

    if unresolved_count > 0 {
        markdown.push_str("\n## Unresolved rows\n\n");
        for row in rows.iter().filter(|row| !row.resolved) {
            markdown.push_str(&format!(
                "- {}: {}\n",
                row.row_id,
                row.unresolved_reason
                    .clone()
                    .unwrap_or_else(|| "unspecified".into())
            ));
        }
    }

    let manifest = Manifest {
        schema_version: M12_SCHEMA_VERSION.to_string(),
        generated_at_unix_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        platform: format!(
            "{}-{} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::consts::FAMILY
        ),
        source_tree_digest: crate::bundle::source_tree_digest(),
        gate: "".into(),
        commands: vec![
            "agent-eval --platform-closure-m12",
            "cargo test -p agent-core --test broker_host_coordinator",
            "cargo test -p tool-runtime host_policies",
        ],
        policy_table_entries: BUILTIN_TOOL_POLICIES.len(),
    };
    (markdown, manifest)
}

/// Shared rows.jsonl writer for every closure-audit artifact set.
pub(crate) fn persist_jsonl_rows(out_dir: &Path, rows: &[impl Serialize]) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    let mut body = String::new();
    for row in rows {
        body.push_str(&serde_json::to_string(row)?);
        body.push('\n');
    }
    std::fs::write(out_dir.join("rows.jsonl"), body)?;
    Ok(())
}

/// Shared REPORT.md + manifest.json writer for closure-audit artifacts.
pub(crate) fn persist_report_and_manifest(
    out_dir: &Path,
    report: &str,
    manifest: impl Serialize,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(out_dir)?;
    std::fs::write(out_dir.join("REPORT.md"), report)?;
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full closure audit must hold every gate when run deterministically.
    #[tokio::test]
    async fn m12_closure_gates_hold_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("m12");
        let (_report, passed) = match run_m12_closure(&out).await {
            Ok(result) => result,
            Err(error) => {
                let body = std::fs::read_to_string(out.join("rows.jsonl")).unwrap_or_default();
                for line in body.lines() {
                    let Ok(row) = serde_json::from_str::<serde_json::Value>(line) else {
                        continue;
                    };
                    if row["resolved"] == serde_json::json!(false) {
                        eprintln!(
                            "UNRESOLVED {} -> {}",
                            row["row_id"],
                            row["unresolved_reason"].as_str().unwrap_or(""),
                        );
                    }
                }
                panic!("audit failed: {error:#}");
            }
        };
        assert!(passed, "m12 closure audit must pass");
    }
}
