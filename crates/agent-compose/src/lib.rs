//! Reusable application/bootstrap composition (COMPOSE-01): one stateless,
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
use provider_openai::{OpenAiConfig, OpenAiProtocol, OpenAiProvider, RetryingTransport};
use tokio::sync::broadcast;

mod compactor;
mod host_policies;
mod mock_model;

pub use host_policies::HostToolPolicyRegistry;

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
pub fn model_from_env() -> Arc<dyn ModelTransport> {
    let Ok(api_key) = std::env::var("OPENAI_API_KEY") else {
        return Arc::new(MockModelTransport);
    };
    if api_key.trim().is_empty() {
        return Arc::new(MockModelTransport);
    }

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
    Arc::new(RetryingTransport::new(
        provider,
        3,
        std::time::Duration::from_millis(500),
    ))
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
    /// LONG-TASK Slice C 候选开关（默认关）：派生 advisory 完成机会并
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
    host.start().await?;

    // The composition seam: every service the run needs is resolved from
    // the host's typed registry and handed to the runtime as one
    // `RuntimeServices`; the kernel is derived inside the runtime.
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
    let instance = RuntimeInstance::spawn(host, services);
    Ok(ComposedRuntime {
        workspace,
        instance,
    })
}
