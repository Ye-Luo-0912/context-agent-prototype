//! Reusable application/bootstrap composition: one stateless,
//! actor-free function turns a `ComposeConfig` into a `RuntimeInstance`
//! wired over the module host. The TUI, any future CLI and the evaluation
//! harness share the same host wiring — context/model/tool/approval
//! modules, the optional event/artifact modules, the
//! `CapabilityAwareDispatcher` and `RuntimeServices` derivation — so a
//! composition change is exercised everywhere it is used.
//!
//! This crate owns no state and runs no loop: `compose` is a pure async
//! function of its inputs (stateless), and it never drives the actor
//! (actor-free — the caller subscribes, starts and drives the returned
//! instance). Like `agent-tui` it is a composition root: it may import
//! every concrete implementation, and nothing below `agent-runtime` may
//! import it.

use std::path::Path;
use std::sync::Arc;

use agent_contracts::{
    AgentResult, ApprovalGate, ContextEngine, ModelTransport, RuntimeEventEnvelope, ToolDispatcher,
};
use agent_core::CoreAuthorityConfig;
use agent_runtime::{
    ApprovalModule, ArtifactModule, AuthorityRecoveryServices, CapabilityAwareDispatcher,
    ContextModule, EventModule, ModelModule, ModuleHost, RuntimeCheckpoint, RuntimeHandle,
    RuntimeInstance, RuntimeServices, ToolModule,
};
use agent_storage::{FileEventJournal, FileOperationJournal};
use agent_workspace::Workspace;
use context_baselines::{AppendOnlyEngine, RollingConfig, RollingSummaryEngine};
use context_contextcore::{ContextServiceConfig, ServiceEngine, connect_engine};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use provider_openai::{
    JsonlRetryObserver, OpenAiConfig, OpenAiProtocol, OpenAiProvider, RetryingTransport,
};
use tokio::sync::broadcast;

mod compactor;
mod host_policies;
mod mock_model;
mod proof_verifier;

pub use host_policies::HostToolPolicyRegistry;
pub use proof_verifier::HostProofVerifier;

pub use compactor::ModelBackedCompactor;
pub use mock_model::MockModelTransport;

/// The context-engine policy, a composition-root choice shared by every
/// entry point (TUI / CLI / eval). `append`, `rolling` and `dynamic` are
/// in-process engines; `service` runs the process-boundary adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPolicy {
    Append,
    Rolling,
    Dynamic,
    Service,
}

impl ContextPolicy {
    /// Parse a `--context=` CLI value; the error names the valid set.
    pub fn from_str_checked(value: &str) -> anyhow::Result<Self> {
        match value {
            "append" => Ok(Self::Append),
            "rolling" => Ok(Self::Rolling),
            "dynamic" => Ok(Self::Dynamic),
            "service" => Ok(Self::Service),
            other => anyhow::bail!(
                "unknown --context policy: {other} (expected append | rolling | dynamic | service)"
            ),
        }
    }

    /// The canonical CLI spelling of this policy.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Append => "append",
            Self::Rolling => "rolling",
            Self::Dynamic => "dynamic",
            Self::Service => "service",
        }
    }
}

/// Build the context engine for a policy. The external context store always
/// lives under the run's state directory — never guessed from the CWD, so a
/// run started from a crate directory does not scatter `.focus-agent`
/// folders around the tree.
///
/// `model` 非空时，rolling / dynamic 注入同一有界压缩器：B 折叠和 C 的
/// episode-rotation semantic distill 共用（`TaskCompleted` 直接写入
/// `CompletionRecord.summary`，不再二次 LLM）。`append` 和
/// 进程外 `service` 不注入（子进程引擎没有这条 in-process 注入缝）。
pub async fn build_context_engine(
    policy: ContextPolicy,
    state_dir: &Path,
    model: Option<Arc<dyn ModelTransport>>,
) -> anyhow::Result<Arc<dyn ContextEngine>> {
    let compactor = model.map(|model| Arc::new(ModelBackedCompactor::new(model)));
    match policy {
        ContextPolicy::Append => Ok(Arc::new(AppendOnlyEngine::new())),
        ContextPolicy::Rolling => {
            let engine = RollingSummaryEngine::with_config(RollingConfig::default());
            Ok(Arc::new(match compactor {
                Some(compactor) => engine.with_compactor(compactor),
                None => engine,
            }))
        }
        ContextPolicy::Dynamic => {
            let engine = SimpleContextEngine::new(SimpleContextConfig {
                context_store_dir: Some(state_dir.join("context-store")),
                ..SimpleContextConfig::default()
            });
            Ok(Arc::new(match compactor {
                Some(compactor) => engine.with_compactor(compactor),
                None => engine,
            }))
        }
        ContextPolicy::Service => connect_engine(&ContextServiceConfig {
            engine: ServiceEngine::Dynamic,
            // The service's context store must live under the workspace
            // state dir too — the child never guesses a CWD-relative path.
            store_dir: Some(state_dir.join("context-store")),
            ..ContextServiceConfig::default()
        })
        .await
        .map_err(anyhow::Error::from),
    }
}

/// Composition-root model selection: a real OpenAI-compatible provider when
/// `OPENAI_API_KEY` is set, otherwise the mock transport.
///
/// Optional overrides:
/// - `OPENAI_BASE_URL` (default `https://api.openai.com/v1`) — point at
///   DeepSeek (`https://api.deepseek.com/v1`), Qwen, Moonshot, GLM, ...
/// - `OPENAI_MODEL` (default `gpt-4o-mini`)
/// - `OPENAI_API_PROTOCOL` (`auto` by default; also `responses` or `chat`)
/// - `OPENAI_CONTEXT_WINDOW` (default 128000 declared send window)
///
/// The model a composition root selected from the process environment.
/// Demo mode is explicit (`AGENT_DEMO=1`); a missing provider key is a
/// configuration error, never a silent mock.
pub enum ModelSelection {
    /// The explicit demo transport (`AGENT_DEMO=1`).
    Mock(Arc<dyn ModelTransport>),
    /// A configured OpenAI-compatible provider transport.
    Provider(Arc<dyn ModelTransport>),
}

impl std::fmt::Debug for ModelSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The transport itself does not implement Debug; never print a
            // key or configuration detail.
            Self::Mock(_) => f.write_str("Mock"),
            Self::Provider(_) => f.write_str("Provider"),
        }
    }
}

/// Checked model configuration: `AGENT_DEMO=1` selects the demo mock
/// explicitly; otherwise `OPENAI_API_KEY` must be present and non-empty or
/// this is a startup error. The historical silent fallback to the mock on
/// a missing key hid unreproducible runs behind a fake model.
pub fn try_model_from_env() -> anyhow::Result<ModelSelection> {
    let demo = std::env::var("AGENT_DEMO")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty());
    match (demo, api_key) {
        (true, _) => Ok(ModelSelection::Mock(Arc::new(MockModelTransport))),
        (false, None) => Err(anyhow::anyhow!(
            "no model configured: set OPENAI_API_KEY (plus OPENAI_BASE_URL / OPENAI_MODEL /              OPENAI_API_PROTOCOL as needed), or set AGENT_DEMO=1 for the explicit demo transport"
        )),
        (false, Some(api_key)) => Ok(ModelSelection::Provider(model_from_key(api_key))),
    }
}

/// Build the retrying provider transport for an already-checked key.
fn model_from_key(api_key: String) -> Arc<dyn ModelTransport> {
    let base_url = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());
    let protocol = std::env::var("OPENAI_API_PROTOCOL")
        .ok()
        .and_then(|value| OpenAiProtocol::parse(&value).ok())
        .unwrap_or_default();
    let context_window = env_declared_context_window();
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key,
        base_url,
        model,
        protocol,
        max_output_tokens: 4096,
        timeout: std::time::Duration::from_secs(120),
        send_stream_options: true,
        send_max_tokens: true,
        max_stream_bytes: provider_openai::DEFAULT_MAX_STREAM_BYTES,
        context_window: Some(context_window),
    });
    Arc::new(
        RetryingTransport::new(provider, 3, std::time::Duration::from_millis(500))
            // Same retry-observability contract as the evaluation harness: set
            // `OPENAI_RETRY_METRICS_FILE` to persist typed incident/stage
            // records; without it the stderr retry line stays the only channel.
            .with_observer(Arc::new(JsonlRetryObserver::from_env())),
    )
}

fn env_declared_context_window() -> usize {
    match std::env::var("OPENAI_CONTEXT_WINDOW") {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse()
            .unwrap_or(provider_openai::DEFAULT_DECLARED_CONTEXT_WINDOW),
        _ => provider_openai::DEFAULT_DECLARED_CONTEXT_WINDOW,
    }
}

/// Everything a composed run differs on. The composition root (TUI/CLI/
/// eval) selects the concrete pieces — engine, model, approval, tools —
/// and `compose` wires them into a runtime.
pub struct ComposeConfig {
    /// Opened workspace (owns the state dir and artifact confinement).
    pub workspace: Workspace,
    /// The context engine, already selected by the caller.
    pub context_engine: Arc<dyn ContextEngine>,
    /// The model transport, already selected by the caller.
    pub model: Arc<dyn ModelTransport>,
    /// The approval gate, already selected by the caller.
    pub approval: Arc<dyn ApprovalGate>,
    /// The base tool dispatcher (the builtin surface).
    pub base_tools: Arc<dyn ToolDispatcher>,
    /// Wrap `base_tools` in a `CapabilityAwareDispatcher` over the host's
    /// capability registry (interactive mode). `false` uses `base_tools`
    /// as-is (harness mode).
    pub capability_aware: bool,
    /// Optional durable event journal -> `EventModule`.
    pub journal: Option<Arc<FileEventJournal>>,
    /// Optional artifact store -> `ArtifactModule`.
    pub artifact_store: Option<Arc<Workspace>>,
    /// Optional output broker (bounds every model-facing tool field and
    /// spills oversized content under the run's artifact directory).
    pub output_broker: Option<Arc<dyn agent_contracts::OutputBroker>>,
    /// Live eval 把内核 tool-loop 上限抬到与 harness 相同的共享 cap。
    /// `None` 保持 `CoreAuthorityConfig` 默认 16（TUI）。不要给 C 比 A 更高的上限。
    pub max_tool_rounds: Option<usize>,
    /// Ablation: omit TaskProgress from the Focus frame. Default true.
    pub project_task_progress: bool,
    /// Project the neutral settlement fact inside TaskProgress. Default
    /// false; it is independent from the product TaskProgress surface so an
    /// experiment cannot also alter Context maintenance inputs.
    pub project_settlement: bool,
    /// Enable the expensive same-state settlement counterfactual audit and
    /// common treatment-sized packing envelope. Default false for every
    /// product and ordinary evaluation composition; paired causal cells set
    /// the same true value in both arms.
    pub settlement_projection_diagnostics: bool,
    /// 完成机会候选开关（默认关）：派生 advisory 完成机会并
    /// 允许一次决策的 `task.complete` 租赁。晋级门通过前保持关。
    pub project_completion_opportunity: bool,
    /// 目录工具准入候选开关（默认关）：类型化缺失父目录失败不改变
    /// 模型表面，`fs.mkdir` 保持 catalog-cold 基线；开启时受信恢复源
    /// 为一次决策精确浮现宿主工具。隔离配对实时门是唯一晋级路径。
    pub recovery_surface: bool,
    /// 受信的宿主授权注册表。缺省只装内置表；插件工具没有条目就没有
    /// 授权，保持 fail-closed。同一来源接入内核配置与能力分发器。
    pub host_policies: Option<Arc<HostToolPolicyRegistry>>,
    /// 预留日志开关（默认关）：给出路径后，每个已批准效果跨持久
    /// 三相屏障，崩溃后启动对账可咨询经纪预留面。晋级语义不变。
    pub effect_reservation_journal: Option<std::path::PathBuf>,
    /// Host verification recipes shared with the `verify.run` tool surface.
    /// When present, `compose` can inject a host proof runner into the
    /// completion gate. Nothing executes unless `project_proof_refresh` is
    /// also enabled (default closed).
    pub verification_recipes: Option<Arc<tool_runtime::VerificationRecipes>>,
    /// Enable the composition-owned exact proof-refresh transaction (default
    /// false). Requires `verification_recipes`: a true flag without the table
    /// fails the composition closed instead of silently disabling.
    pub project_proof_refresh: bool,
}

/// A composed runtime. Owns the workspace and the spawned `RuntimeInstance`
/// (not yet started — subscribe first, then `start`, so `RunStarted` is
/// observable, exactly like the hand-wired entry points). The caller drives
/// the actor through the handle; `shutdown` runs the full ordered teardown.
pub struct ComposedRuntime {
    pub workspace: Workspace,
    pub instance: RuntimeInstance,
}

impl ComposedRuntime {
    /// The actor handle (user messages, focus and task commands). Complete
    /// checkpoints are owned by `ComposedRuntime`/`RuntimeInstance`, because
    /// only they can include the host capability plane.
    pub fn handle(&self) -> &RuntimeHandle {
        self.instance.handle()
    }

    /// Capture actor/context/capability state plus the durable Core authority
    /// prefix marker. Operation truth remains in the authority WAL.
    pub async fn checkpoint(&self) -> AgentResult<RuntimeCheckpoint> {
        self.instance.checkpoint().await
    }

    /// Subscribe to runtime events. Call before `start` to see `RunStarted`.
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEventEnvelope> {
        self.instance.handle().subscribe()
    }

    /// Full ordered shutdown (actor -> host -> join), aggregating errors.
    pub async fn shutdown(self) -> AgentResult<()> {
        self.instance.shutdown().await
    }
}

/// Wire the module host (context/model/tool/approval + optional event/
/// artifact modules), derive the kernel services from the typed registry,
/// and spawn the runtime actor over them. Stateless: a pure function of
/// `config`; the actor is spawned but not started, so the caller can
/// subscribe to events before `start`.
pub async fn compose(config: ComposeConfig) -> anyhow::Result<ComposedRuntime> {
    let ComposeConfig {
        workspace,
        context_engine,
        model,
        approval,
        base_tools,
        capability_aware,
        journal,
        artifact_store,
        output_broker,
        max_tool_rounds,
        project_task_progress,
        project_settlement,
        settlement_projection_diagnostics,
        project_completion_opportunity,
        recovery_surface,
        host_policies,
        effect_reservation_journal,
        verification_recipes,
        project_proof_refresh,
    } = config;

    // 授权映射是组合根的决定：内置表加运维准入的插件绑定，内核与
    // 分发器共用一份。
    let host_policies =
        host_policies.unwrap_or_else(|| Arc::new(HostToolPolicyRegistry::with_builtins()));

    let mut host = ModuleHost::new();
    host.add_module(Arc::new(ContextModule::new(context_engine)))?;
    host.add_module(Arc::new(ModelModule::new(model)))?;
    // The capability registry is the host's: capabilities registered against
    // it (even mid-run) are picked up by the tool provider on the next
    // model request. The dispatcher must see it before the ToolModule is
    // added.
    let capability_registry = host.capability_registry();
    let tools: Arc<dyn ToolDispatcher> = if capability_aware {
        Arc::new(
            CapabilityAwareDispatcher::with_workspace(
                base_tools,
                capability_registry,
                // Capabilities that declare workspace/artifact permissions
                // receive confined handles into the same workspace the builtin
                // tools use.
                Some(Arc::new(workspace.clone())),
            )
            .with_host_policies(host_policies.clone()),
        )
    } else {
        base_tools
    };
    host.add_module(Arc::new(ToolModule::new(tools)))?;
    host.add_module(Arc::new(ApprovalModule::new(approval)))?;
    if let Some(journal) = journal {
        host.add_module(Arc::new(EventModule::new(journal)))?;
    }
    if let Some(artifact_store) = artifact_store {
        host.add_module(Arc::new(ArtifactModule::new(artifact_store)))?;
    }

    // All fallible preparation runs before any module starts, so a
    // preparation failure can never leave a serving child or a half-built
    // runtime behind: the host is started only after the journal, broker,
    // authority and service set are fully constructed.
    let operation_journal = Arc::new(
        FileOperationJournal::open(
            workspace
                .state_dir()
                .join("authority")
                .join("operations.jsonl"),
        )?
        .0,
    );
    let mut authority = CoreAuthorityConfig {
        output_broker,
        host_policies: Some(host_policies),
        ..CoreAuthorityConfig::default()
    };
    if let Some(max_tool_rounds) = max_tool_rounds {
        authority.max_tool_rounds = max_tool_rounds;
    }
    // 预留日志开关（默认关）：开启后每个已批准效果跨持久三相屏障，
    // 崩溃后启动对账可咨询经纪预留面；关闭时保持内联行为不变。
    if let Some(journal_path) = effect_reservation_journal {
        let journaled = agent_core::JournaledEffectBroker::open(
            Arc::new(agent_core::LocalEffectBroker),
            &journal_path,
        )?;
        authority.effect_broker = Some(Arc::new(journaled));
    }
    let mut services = RuntimeServices::from_registry_with_operation_journal(
        host.registry(),
        authority,
        AuthorityRecoveryServices::new(operation_journal, Some(Arc::new(workspace.clone()))),
    )?;
    services = services.with_project_task_progress(project_task_progress);
    services = services.with_project_settlement(project_settlement);
    services = services.with_settlement_projection_diagnostics(settlement_projection_diagnostics);
    services = services.with_project_completion_opportunity(project_completion_opportunity);
    services = services.with_recovery_surface(recovery_surface);
    // A recipe table always installs the read-only domain resolver so a
    // model-facing repair can name an exact recipe on cold start. The switch
    // controls only Runtime's optional automatic execution of that recipe.
    // Enabling execution without a table remains a fail-closed boot error.
    match verification_recipes {
        Some(recipes) => {
            let runner = tool_runtime::RecipeProofRunner::new(workspace.clone(), recipes)
                .ok_or_else(|| anyhow::anyhow!("verification recipes register no host policy"))?;
            services = services.with_proof_verifier(Arc::new(HostProofVerifier::new(runner)));
            if project_proof_refresh {
                services = services.with_project_proof_refresh(true);
            }
        }
        None if project_proof_refresh => {
            return Err(anyhow::anyhow!(
                "project proof refresh requires host verification recipes"
            ));
        }
        None => {}
    }

    // Everything fallible is constructed; only the module start transaction
    // and the startup store reconcile remain, and both are rolled back
    // closed when they fail.
    host.start().await?;
    if let Err(error) = host.registry().context_service()?.reconcile_store().await {
        // The store reconcile races nothing yet (the actor is not spawned),
        // but it is the last post-start seam: stop every started module
        // before reporting the failure so no serving child survives.
        let stop_error = host.stop().await.err();
        let message = match stop_error {
            Some(stop_error) => format!(
                "startup store reconcile failed: {error}; module stop also failed: {stop_error}"
            ),
            None => format!("startup store reconcile failed: {error}"),
        };
        return Err(anyhow::anyhow!(message));
    }

    let instance = RuntimeInstance::spawn(host, services);
    Ok(ComposedRuntime {
        workspace,
        instance,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ApprovalGate, ContextEngine, ModelTransport, ToolDispatcher};
    use agent_core::PolicyApprovalGate;
    use agent_runtime::ProofVerifier;
    use context_simple::{SimpleContextConfig, SimpleContextEngine};
    use tool_runtime::{BuiltinToolDispatcher, VerificationRecipe, VerificationRecipes};

    fn host_echo_recipe() -> VerificationRecipe {
        #[cfg(windows)]
        let argv = vec![
            "cmd".into(),
            "/C".into(),
            "echo".into(),
            "host-proof".into(),
        ];
        #[cfg(not(windows))]
        let argv = vec!["echo".into(), "host-proof".into()];
        VerificationRecipe::new("host.echo", "Echo host proof marker", "v1", argv)
            .unwrap()
            .with_exact_current_world_reuse()
    }

    fn compose_config(
        workspace: Workspace,
        recipes: Option<Arc<VerificationRecipes>>,
        refresh: bool,
    ) -> ComposeConfig {
        let engine: Arc<dyn ContextEngine> =
            Arc::new(SimpleContextEngine::new(SimpleContextConfig::default()));
        let model: Arc<dyn ModelTransport> = Arc::new(MockModelTransport);
        let approval: Arc<dyn ApprovalGate> = Arc::new(PolicyApprovalGate::permissive());
        let base_tools: Arc<dyn ToolDispatcher> =
            Arc::new(BuiltinToolDispatcher::new(workspace.clone()).unwrap());
        ComposeConfig {
            workspace,
            context_engine: engine,
            model,
            approval,
            base_tools,
            capability_aware: false,
            journal: None,
            artifact_store: None,
            output_broker: None,
            max_tool_rounds: None,
            project_task_progress: true,
            project_settlement: false,
            settlement_projection_diagnostics: false,
            project_completion_opportunity: false,
            recovery_surface: false,
            host_policies: None,
            effect_reservation_journal: None,
            verification_recipes: recipes,
            project_proof_refresh: refresh,
        }
    }

    async fn run_smoke(config: ComposeConfig) {
        let composed = compose(config).await.unwrap();
        composed.instance.start().await.unwrap();
        composed.shutdown().await.unwrap();
    }

    /// The default composition injects no host verifier and keeps the
    /// proof-refresh switch off: existing compositions are unchanged.
    #[tokio::test]
    async fn default_composition_keeps_proof_refresh_closed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        run_smoke(compose_config(workspace, None, false)).await;
    }

    /// Enabling the switch without the recipe table fails the composition
    /// closed; a silently disabled gate is worse than a refused boot.
    #[tokio::test]
    async fn enabled_refresh_without_recipes_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let error = match compose(compose_config(workspace, None, true)).await {
            Ok(_) => panic!("composition with refresh but no recipes must fail closed"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("verification recipes"),
            "{error}"
        );
    }

    /// A recipe table injects the route resolver even with automatic refresh
    /// disabled; enabling refresh changes execution policy, not discovery.
    #[tokio::test]
    async fn disabled_refresh_with_recipes_still_composes_the_route_resolver() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(vec![host_echo_recipe()]).unwrap());
        run_smoke(compose_config(workspace, Some(recipes), false)).await;
    }

    /// With the recipe table present the enabled composition boots and
    /// injects the host executor; the model tool surface is unchanged.
    #[tokio::test]
    async fn enabled_refresh_with_recipes_composes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(vec![host_echo_recipe()]).unwrap());
        run_smoke(compose_config(workspace, Some(recipes), true)).await;
    }

    /// The adapter maps one host proof run onto the runtime verifier
    /// contract: outcome and identity pass through unchanged.
    #[tokio::test]
    async fn host_proof_verifier_maps_runner_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(vec![host_echo_recipe()]).unwrap());
        let runner = tool_runtime::RecipeProofRunner::new(workspace, recipes).unwrap();
        let verifier = HostProofVerifier::new(runner);
        let outcome = verifier
            .verify_exact(agent_runtime::ProofVerifierRequest {
                run_id: agent_contracts::RunId::new(),
                task_id: agent_contracts::TaskId::new(),
                recipe_id: "host.echo".into(),
                verification_revision: 1,
                directive_revision: 1,
                workspace_revision: 1,
            })
            .await
            .unwrap();
        assert!(outcome.ok, "{}", outcome.summary);
        assert!(!outcome.verification_identity.is_empty());
        assert_eq!(outcome.verification_identity.len(), 64);
    }

    /// A context engine that fails the startup store reconcile — the single
    /// post-start seam in `compose` — so a test can prove the rollback guard
    /// stops every module instead of handing out a half-built runtime.
    struct ReconcileFaultEngine {
        inner: Arc<SimpleContextEngine>,
    }

    #[async_trait::async_trait]
    impl ContextEngine for ReconcileFaultEngine {
        async fn ingest(&self, ingress: agent_contracts::ContextIngress) -> AgentResult<()> {
            self.inner.ingest(ingress).await
        }
        async fn maintain(
            &self,
            trigger: agent_contracts::ContextMaintenanceTrigger,
        ) -> AgentResult<agent_contracts::ContextMaintenanceReport> {
            self.inner.maintain(trigger).await
        }
        async fn materialize(
            &self,
            query: agent_contracts::ContextQuery,
        ) -> AgentResult<agent_contracts::MaterializedContext> {
            self.inner.materialize(query).await
        }
        async fn open_scope(
            &self,
            kind: agent_contracts::ScopeKind,
            parent: Option<agent_contracts::ScopeId>,
        ) -> AgentResult<agent_contracts::ScopeId> {
            self.inner.open_scope(kind, parent).await
        }
        async fn close_scope(
            &self,
            scope_id: agent_contracts::ScopeId,
        ) -> AgentResult<Vec<agent_contracts::ContextStateTransition>> {
            self.inner.close_scope(scope_id).await
        }
        async fn diagnostics(&self) -> AgentResult<agent_contracts::ContextDiagnostics> {
            self.inner.diagnostics().await
        }
        async fn inspect(
            &self,
            limit: usize,
        ) -> AgentResult<Vec<agent_contracts::ContextItemSummary>> {
            self.inner.inspect(limit).await
        }
        async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
            self.inner.checkpoint().await
        }
        async fn restore(&self, data: serde_json::Value) -> AgentResult<()> {
            self.inner.restore(data).await
        }
        async fn reconcile_store(&self) -> AgentResult<agent_contracts::StoreReconcileReport> {
            Err(agent_contracts::AgentError::Internal(
                "simulated startup store reconcile failure".into(),
            ))
        }
    }

    /// The only post-start seam rolls every started module back and returns
    /// an error: a failed composition never yields an instance.
    #[tokio::test]
    async fn failed_post_start_seam_rolls_back_and_leaves_no_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let mut config = compose_config(workspace, None, false);
        config.context_engine = Arc::new(ReconcileFaultEngine {
            inner: Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        });
        let error = match compose(config).await {
            Ok(_) => panic!("a failed startup reconcile must fail the composition"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("startup store reconcile failed"),
            "{error}"
        );
    }

    /// Every fallible preparation step runs before the host starts, so a
    /// journal that cannot be opened locks the composition down without any
    /// module reaching the serving state.
    #[tokio::test]
    async fn locked_effect_journal_fails_before_any_module_starts() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let journal_path = dir.path().join("journal").join("effect-reservations.jsonl");
        // Hold the exclusive journal lock so the composition's own open must
        // fail closed before any module starts.
        let _holder = agent_core::ReservationJournal::open(&journal_path).unwrap();
        let mut config = compose_config(workspace, None, false);
        config.effect_reservation_journal = Some(journal_path);
        let error = match compose(config).await {
            Ok(_) => panic!("a locked effect journal must fail the composition"),
            Err(error) => error,
        };
        assert!(!error.to_string().is_empty());
    }
}

#[cfg(test)]
mod model_selection_tests {
    use super::*;

    /// Env mutations are process-global; serialize every selection test.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            let saved = keys
                .iter()
                .map(|key| (*key, std::env::var(key).ok()))
                .collect();
            for key in keys {
                // Test-only, serialized under ENV_LOCK: the process is
                // single-threaded with respect to these keys.
                unsafe { std::env::remove_var(key) };
            }
            EnvGuard { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
        }
    }

    #[test]
    fn a_missing_key_without_explicit_demo_is_a_configuration_error() {
        let _env = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(&["OPENAI_API_KEY", "AGENT_DEMO"]);
        let error = try_model_from_env().unwrap_err().to_string();
        assert!(error.contains("no model configured"), "{error}");
        assert!(error.contains("AGENT_DEMO=1"), "{error}");
    }

    #[test]
    fn demo_mode_is_explicit_and_wins_over_an_absent_key() {
        let _env = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(&["OPENAI_API_KEY", "AGENT_DEMO"]);
        unsafe { std::env::set_var("AGENT_DEMO", "1") };
        assert!(matches!(
            try_model_from_env().unwrap(),
            ModelSelection::Mock(_)
        ));
    }

    #[test]
    fn a_present_key_selects_the_provider_transport() {
        let _env = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(&["OPENAI_API_KEY", "AGENT_DEMO"]);
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test") };
        assert!(matches!(
            try_model_from_env().unwrap(),
            ModelSelection::Provider(_)
        ));
    }
}
