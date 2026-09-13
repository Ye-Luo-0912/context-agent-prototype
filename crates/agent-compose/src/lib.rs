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
    AgentResult, ApprovalGate, BoundedCompactor, ContextEngine, ModelTransport,
    RuntimeEventEnvelope, ToolDispatcher,
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
    JsonlRetryObserver, OpenAiConfig, OpenAiPromptCacheMode, OpenAiProtocol, OpenAiProvider,
    ResponsesReasoningEffort, RetryingTransport,
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
///
/// **CTX-4 产品 profile 决策（2026-09-12）**：生产默认保持
/// `ContextPolicy::Rolling`（宿主未指定时的选择），最低正确性义务由
/// baseline 引擎的 `required_claim_misses`（CORE-2：每个 PromptRequired
/// 如实报 Missing）＋runtime 的模型可读 required-miss 呈现（CTX-4）承担
/// ——不把「不支持必需正文投影」伪装成「全部已满足」。切换 Dynamic 是
/// 质量/产品决策，须先通过 CTX-4 的真实默认入口旅程验收；实验 baseline
/// （append/rolling）的可比语义保持不变。
pub async fn build_context_engine(
    policy: ContextPolicy,
    state_dir: &Path,
    model: Option<Arc<dyn ModelTransport>>,
    maintenance_model: Option<Arc<dyn ModelTransport>>,
    budget: &MaintenanceBudget,
) -> anyhow::Result<Arc<dyn ContextEngine>> {
    // COST-4 (D02): the optional maintenance transport owns the compactor —
    // its timeout (and the request-level output cap) are the maintenance
    // call's own bounds instead of the main profile's. Absent, the main
    // model serves (the historical single-transport behavior).
    // COST-8: a ZERO budget (calls or tokens) means the compactor is not
    // attached at all — zero budget never sends, and the budget's count/
    // token/backoff values flow into the engine's own per-pass limits.
    let compactor = if budget.allows_calls() {
        maintenance_model
            .or(model)
            .map(|model| Arc::new(ModelBackedCompactor::new(model)) as Arc<dyn BoundedCompactor>)
    } else {
        None
    };
    match policy {
        ContextPolicy::Append => Ok(Arc::new(AppendOnlyEngine::new())),
        ContextPolicy::Rolling => {
            let engine = RollingSummaryEngine::with_config(RollingConfig {
                max_compactor_calls_per_maintain: budget.max_calls_per_maintain,
                max_compactor_tokens_per_maintain: budget.max_tokens_per_maintain,
                compact_failure_backoff_maintains: budget.compact_failure_backoff_maintains,
                ..RollingConfig::default()
            });
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
    /// A configured OpenAI-compatible provider transport, carrying the
    /// checked key-free serving identity.
    Provider(Arc<dyn ModelTransport>, ProviderProfile),
}

impl std::fmt::Debug for ModelSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The transport itself does not implement Debug; never print a
            // key or configuration detail.
            Self::Mock(_) => f.write_str("Mock"),
            Self::Provider(_, _) => f.write_str("Provider"),
        }
    }
}

/// The checked, key-free identity of the configured serving: exactly the
/// fields that change model behavior. The digest is stable across runs,
/// never contains the API key, and is safe to print, log, or persist.
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    pub base_url: String,
    pub model: String,
    pub protocol: &'static str,
    pub context_window: usize,
    pub max_output_tokens: usize,
    pub sampling: provider_openai::SamplingPolicy,
    pub prompt_cache_mode: OpenAiPromptCacheMode,
    pub responses_reasoning_effort: ResponsesReasoningEffort,
}

impl ProviderProfile {
    /// SHA-256 over the canonical identity JSON. Two runs compare their
    /// operating points by comparing this string.
    pub fn digest(&self) -> String {
        use sha2::{Digest as _, Sha256};
        let mut identity = serde_json::json!({
            "schema": "provider-profile.v1",
            "base_url": self.base_url,
            "model": self.model,
            "protocol": self.protocol,
            "context_window": self.context_window,
            "max_output_tokens": self.max_output_tokens,
            "sampling": match self.sampling {
                provider_openai::SamplingPolicy::ProviderDefault => {
                    serde_json::json!("provider-default")
                }
                provider_openai::SamplingPolicy::Temperature(temperature) => {
                    serde_json::json!({ "temperature": temperature })
                }
            },
        });
        // Preserve historical/default serving identities. Only an explicit
        // change to wire caching adds a new identity dimension.
        if self.prompt_cache_mode != OpenAiPromptCacheMode::ProviderDefault {
            identity["prompt_cache_mode"] = serde_json::json!(self.prompt_cache_mode.as_str());
        }
        if self.responses_reasoning_effort != ResponsesReasoningEffort::ProviderDefault {
            identity["responses_reasoning_effort"] =
                serde_json::json!(self.responses_reasoning_effort.as_str());
        }
        let digest = Sha256::digest(identity.to_string().as_bytes());
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            hex.push_str(&format!("{byte:02x}"));
        }
        hex
    }

    /// One-line serving identity for a startup banner.
    pub fn banner(&self) -> String {
        let digest = self.digest();
        format!(
            "provider profile: {} @ {} protocol={} context_window={} max_output_tokens={} sampling={} prompt_cache={} responses_reasoning={} digest={}",
            self.model,
            self.base_url,
            self.protocol,
            self.context_window,
            self.max_output_tokens,
            self.sampling.description(),
            self.prompt_cache_mode.as_str(),
            self.responses_reasoning_effort.as_str(),
            &digest[..16],
        )
    }
}

fn protocol_name(protocol: OpenAiProtocol) -> &'static str {
    match protocol {
        OpenAiProtocol::Auto => "auto",
        OpenAiProtocol::Responses => "responses",
        OpenAiProtocol::ChatCompletions => "chat",
    }
}

/// Checked model configuration: `AGENT_DEMO=1` selects the demo mock
/// explicitly; otherwise `OPENAI_API_KEY` must be present and non-empty or
/// this is a startup error. The historical silent fallback to the mock on
/// a missing key hid unreproducible runs behind a fake model. Every other
/// provider variable is parsed strictly: an invalid value is a startup
/// error, never a silent default.
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
        (false, Some(api_key)) => {
            let (transport, profile) = provider_from_env(api_key)?;
            Ok(ModelSelection::Provider(transport, profile))
        }
    }
}

/// COST-8: the product-facing MAINTENANCE budget — how much one execution
/// segment may spend on compaction and how it recovers from a failing
/// source. The budget lives in the composition root (env at the host/TUI/
/// CLI entries), flows into the engine's rolling config, and its effective
/// values are printed in the startup banner so a run's spending limits are
/// checkable, not folklore.
///
/// Boundaries, explicitly named: `max_calls_per_maintain` bounds one
/// maintain pass's serial compactor CALLS; `max_tokens_per_maintain` bounds
/// one pass's cumulative in+out tokens (observed/estimated; unknown rows
/// carry no numbers and stay covered by the call budget); neither claims a
/// whole-execution-segment cap — separate passes each spend up to the
/// budget again. `compact_failure_backoff_maintains` defers a FAILED fold
/// request for that many fold-eligible passes; changed folded content
/// invalidates the deferral immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceBudget {
    /// Compactor calls one maintain pass may make. `0` = never send.
    pub max_calls_per_maintain: usize,
    /// Cumulative in+out tokens one maintain pass may spend. `0` = never
    /// send.
    pub max_tokens_per_maintain: u64,
    /// Fold-eligible passes a failed fold request stays deferred.
    pub compact_failure_backoff_maintains: u32,
}

impl Default for MaintenanceBudget {
    fn default() -> Self {
        Self {
            max_calls_per_maintain: 4,
            // Unlimited by default — an explicit token budget stays opt-in,
            // and the default is NOT reported as "costs are capped".
            max_tokens_per_maintain: u64::MAX,
            compact_failure_backoff_maintains: 4,
        }
    }
}

impl MaintenanceBudget {
    /// Whether this budget lets any compactor call happen at all.
    pub fn allows_calls(&self) -> bool {
        self.max_calls_per_maintain > 0 && self.max_tokens_per_maintain > 0
    }

    /// The key-free, checkable one-line description for banners and
    /// run metadata.
    pub fn describe(&self) -> String {
        let tokens = if self.max_tokens_per_maintain == u64::MAX {
            "unbounded".to_string()
        } else {
            self.max_tokens_per_maintain.to_string()
        };
        format!(
            "maintenance budget: <= {} compactor call(s) and <= {tokens} token(s) per maintain pass; failed folds back off {} pass(es)",
            self.max_calls_per_maintain, self.compact_failure_backoff_maintains
        )
    }
}

/// Reads the maintenance budget from the process environment. Unset
/// variables keep the defaults; a variable that fails to parse is a
/// startup error, never a silent fallback:
/// - `MAINTENANCE_MAX_CALLS_PER_MAINTAIN` (usize, 0 = disable compaction)
/// - `MAINTENANCE_MAX_TOKENS_PER_MAINTAIN` (u64, 0 = disable compaction)
/// - `MAINTENANCE_COMPACT_FAILURE_BACKOFF` (u32 passes, 0 = retry at once)
pub fn maintenance_budget_from_env() -> anyhow::Result<MaintenanceBudget> {
    let mut budget = MaintenanceBudget::default();
    if let Some(calls) = env_checked("MAINTENANCE_MAX_CALLS_PER_MAINTAIN", |raw| {
        raw.parse::<usize>()
            .map_err(|_| format!("must be a non-negative integer, got '{raw}'"))
    })? {
        budget.max_calls_per_maintain = calls;
    }
    if let Some(tokens) = env_checked("MAINTENANCE_MAX_TOKENS_PER_MAINTAIN", |raw| {
        raw.parse::<u64>()
            .map_err(|_| format!("must be a non-negative integer, got '{raw}'"))
    })? {
        budget.max_tokens_per_maintain = tokens;
    }
    if let Some(backoff) = env_checked("MAINTENANCE_COMPACT_FAILURE_BACKOFF", |raw| {
        raw.parse::<u32>()
            .map_err(|_| format!("must be a non-negative integer, got '{raw}'"))
    })? {
        budget.compact_failure_backoff_maintains = backoff;
    }
    Ok(budget)
}

/// COST-4 (D02): an OPTIONAL independent maintenance transport. When
/// `MAINTENANCE_TIMEOUT_SECS` is set (and provider credentials are present),
/// compaction calls run on their own transport with that timeout —
/// maintenance time is bounded by its own profile instead of the main
/// model's 120 s. The retry budget is inherited by design (bounded, and
/// model selection stays out until a semantic regression asks for it).
/// Demo mode never returns a transport: the mock does not bill.
pub fn try_maintenance_transport_from_env() -> anyhow::Result<Option<Arc<dyn ModelTransport>>> {
    let timeout_secs = env_checked("MAINTENANCE_TIMEOUT_SECS", |raw| {
        raw.parse::<u64>()
            .map_err(|_| format!("must be an integer >= 1, got '{raw}'"))
    })?;
    let Some(timeout_secs) = timeout_secs else {
        return Ok(None);
    };
    if timeout_secs == 0 {
        return Err(anyhow::anyhow!("MAINTENANCE_TIMEOUT_SECS: must be >= 1"));
    }
    let demo = std::env::var("AGENT_DEMO")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if demo {
        return Ok(None);
    }
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "MAINTENANCE_TIMEOUT_SECS is set but no provider is configured: set OPENAI_API_KEY"
            )
        })?;
    let (transport, _profile) = provider_from_env_with_timeout(
        api_key,
        Some(std::time::Duration::from_secs(timeout_secs)),
    )?;
    Ok(Some(transport))
}

/// One optional environment string that must parse when present.
fn env_checked<T>(
    name: &str,
    parse: impl Fn(&str) -> Result<T, String>,
) -> anyhow::Result<Option<T>> {
    match std::env::var(name) {
        Err(_) => Ok(None),
        Ok(raw) if raw.trim().is_empty() => Err(anyhow::anyhow!(
            "{name} is set but empty; remove it or give it a value"
        )),
        Ok(raw) => parse(raw.trim())
            .map(Some)
            .map_err(|error| anyhow::anyhow!("{name}: {error}")),
    }
}

fn env_usize(name: &str, default: usize, min: usize) -> anyhow::Result<usize> {
    Ok(env_checked(name, |raw| {
        raw.parse::<usize>()
            .map_err(|_| format!("must be an integer >= {min}, got '{raw}'"))
    })?
    .map(|value| {
        if value < min {
            Err(anyhow::anyhow!(
                "{name}: must be an integer >= {min}, got {value}"
            ))
        } else {
            Ok(value)
        }
    })
    .transpose()?
    .unwrap_or(default))
}

/// Build the retrying provider transport plus its checked identity for an
/// already-checked key. Invalid configuration fails here, before any
/// workspace or runtime state exists.
fn provider_from_env(
    api_key: String,
) -> anyhow::Result<(Arc<dyn ModelTransport>, ProviderProfile)> {
    provider_from_env_with_timeout(api_key, None)
}

/// COST-4 (D02): `timeout_override` lets the optional maintenance transport
/// carry its own time bound; the main-model path keeps the profile default.
fn provider_from_env_with_timeout(
    api_key: String,
    timeout_override: Option<std::time::Duration>,
) -> anyhow::Result<(Arc<dyn ModelTransport>, ProviderProfile)> {
    let base_url = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let model = std::env::var("OPENAI_MODEL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "gpt-4o-mini".to_string());
    let protocol = env_checked("OPENAI_API_PROTOCOL", OpenAiProtocol::parse)?.unwrap_or_default();
    let prompt_cache_mode =
        env_checked("OPENAI_PROMPT_CACHE_MODE", OpenAiPromptCacheMode::parse)?.unwrap_or_default();
    prompt_cache_mode
        .validate_protocol(protocol)
        .map_err(anyhow::Error::msg)?;
    let responses_reasoning_effort = env_checked(
        "OPENAI_RESPONSES_REASONING_EFFORT",
        ResponsesReasoningEffort::parse,
    )?
    .unwrap_or_default();
    responses_reasoning_effort
        .validate_protocol(protocol)
        .map_err(anyhow::Error::msg)?;
    let context_window = env_usize(
        "OPENAI_CONTEXT_WINDOW",
        provider_openai::DEFAULT_DECLARED_CONTEXT_WINDOW,
        1024,
    )?;
    let max_output_tokens = env_usize("OPENAI_MAX_OUTPUT_TOKENS", 4096, 1)?;
    let sampling = env_checked("OPENAI_TEMPERATURE", |raw| {
        let temperature: f32 = raw
            .parse()
            .map_err(|_| format!("must be a number between 0.0 and 2.0, got '{raw}'"))?;
        if !(0.0..=2.0).contains(&temperature) {
            return Err(format!(
                "must be a number between 0.0 and 2.0, got {temperature}"
            ));
        }
        Ok(provider_openai::SamplingPolicy::Temperature(temperature))
    })?
    .unwrap_or_default();
    let profile = ProviderProfile {
        base_url: base_url.clone(),
        model: model.clone(),
        protocol: protocol_name(protocol),
        context_window,
        max_output_tokens,
        sampling,
        prompt_cache_mode,
        responses_reasoning_effort,
    };
    let provider = OpenAiProvider::new(OpenAiConfig {
        api_key,
        base_url,
        model,
        protocol,
        max_output_tokens: profile.max_output_tokens,
        timeout: timeout_override.unwrap_or(std::time::Duration::from_secs(120)),
        send_stream_options: true,
        send_max_tokens: true,
        max_stream_bytes: provider_openai::DEFAULT_MAX_STREAM_BYTES,
        context_window: Some(context_window),
        sampling: profile.sampling,
    })
    .with_prompt_cache_mode(prompt_cache_mode)
    .map_err(anyhow::Error::msg)?
    .with_responses_reasoning_effort(responses_reasoning_effort)
    .map_err(anyhow::Error::msg)?;
    let transport = Arc::new(
        RetryingTransport::new(provider, 3, std::time::Duration::from_millis(500))
            // Same retry-observability contract as the evaluation harness: set
            // `OPENAI_RETRY_METRICS_FILE` to persist typed incident/stage
            // records; without it the stderr retry line stays the only channel.
            .with_observer(Arc::new(JsonlRetryObserver::from_env())),
    );
    Ok((transport, profile))
}

/// Everything a composed run differs on. The composition root (TUI/CLI/
/// eval) selects the concrete pieces — engine, model, approval, tools —
/// and `compose` wires them into a runtime.
pub struct ComposeConfig {
    /// Opened workspace (owns the state dir and artifact confinement).
    pub workspace: Workspace,
    /// Key-free serving identity (`ProviderProfile::digest`) persisted
    /// into every checkpoint's run metadata. `None` for demo/mock or
    /// harness compositions that record the tuple in their own manifests.
    pub provider_profile_digest: Option<String>,
    /// Opt-in: run the completion-time host proof refresh outside the
    /// actor loop and resume the parked completion when it finishes.
    /// `false` (default) keeps the historical inline refresh with its
    /// same-round authoritative gate result.
    pub defer_proof_refresh: bool,
    /// Opt-in: compile the shadow Context Frame manifest each round and
    /// emit it as `ContextFrameShadow` diagnostics. Never changes model
    /// input; measurement only.
    pub shadow_context_frame: bool,
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
    /// M17-B1: the caller's Unix host-death containment decision. The
    /// composition root injects the SAME policy into the host proof lane
    /// that the caller gave the tool dispatcher, so a plain dispatcher with
    /// containment armed never coexists with an unwired proof runner
    /// (F03). Only a binary whose `main` dispatches on
    /// `agent_process::watchdog::WATCHDOG_ENV` may set this.
    pub host_death_watchdog: bool,
    /// E1: MCP stdio servers to connect, discover and register as
    /// capabilities before the modules start. Each declaration is
    /// discovered once at startup (fail-closed: a server that cannot be
    /// reached fails the composition); its tools enter the on-demand
    /// capability catalog and are loaded per task like any other
    /// capability — never injected wholesale into a request.
    pub mcp_servers: Vec<agent_capability_process::McpServerDecl>,
    /// E1: the installed plugin catalog. Wired into the capability
    /// dispatcher so `capability.manage read_skill` can serve on-demand,
    /// bounded skill bodies. `None` (default) leaves skill reads refused.
    pub plugins: Option<Arc<agent_runtime::PluginRegistry>>,
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
    compose_with_prompt_layout(config, agent_contracts::PromptLayout::CurrentStateLast).await
}

/// Use an explicit message layout, including the historical layout for
/// endpoint compatibility or a behavior comparison. Context and authority
/// configuration remain exactly those supplied in `config`.
pub async fn compose_with_prompt_layout(
    config: ComposeConfig,
    prompt_layout: agent_contracts::PromptLayout,
) -> anyhow::Result<ComposedRuntime> {
    let ComposeConfig {
        provider_profile_digest: provider_profile_digest_in,
        defer_proof_refresh,
        shadow_context_frame,
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
        host_death_watchdog,
        mcp_servers,
        plugins,
    } = config;

    // 授权映射是组合根的决定：内置表加运维准入的插件绑定，内核与
    // 分发器共用一份。
    let host_policies =
        host_policies.unwrap_or_else(|| Arc::new(HostToolPolicyRegistry::with_builtins()));

    let mut host = ModuleHost::new();
    // PROCESS-01 / M17-B1: before anything starts, reconcile the host-child
    // supervision ledger — a crashed prior run may have left a verifier or
    // command process alive. The reconciliation is typed: an unreadable or
    // corrupt ledger, a row without a usable identity, or a kill whose exit
    // could not be confirmed all refuse startup instead of being conflated
    // with "no pending children". The host decides the blocked-recovery
    // posture; cleanup success is only ever reported after confirmation.
    let supervision_outcome = tool_runtime::supervision::reconcile_children(workspace.state_dir())
        .map_err(|error| {
            anyhow::anyhow!(
                "supervision ledger at {} could not be reconciled: {error}; \
                 cannot prove no host child was left behind, refusing to reuse this workspace \
                 (resolve the ledger manually before starting)",
                tool_runtime::supervision::ledger_path(workspace.state_dir()).display()
            )
        })?;
    if !supervision_outcome.is_clean() {
        return Err(anyhow::anyhow!(
            "supervision ledger holds unresolved host-child records \
             (unverified: {:?}, unconfirmed: {:?}); resolve these processes manually, then \
             clear their rows from {} before reusing this workspace",
            supervision_outcome.unverified,
            supervision_outcome.unconfirmed,
            tool_runtime::supervision::ledger_path(workspace.state_dir()).display()
        ));
    }
    host.add_module(Arc::new(ContextModule::new(context_engine)))?;
    host.add_module(Arc::new(ModelModule::new(model)))?;
    // The capability registry is the host's: capabilities registered against
    // it (even mid-run) are picked up by the tool provider on the next
    // model request. The dispatcher must see it before the ToolModule is
    // added.
    let capability_registry = host.capability_registry();
    // E1: connect, discover and register every configured MCP server before
    // any module starts — a server that cannot be reached fails the
    // composition (the operator explicitly configured it), and discovery
    // only establishes the static manifest: the server child is reaped and
    // re-spawned lazily on first invoke. Tools enter the on-demand catalog
    // (loaded per task, never injected wholesale); risk derives from the
    // DECLARED permissions, never from the server's self-description.
    for decl in &mcp_servers {
        let risk = if decl
            .permissions
            .iter()
            .any(|p| p == agent_contracts::WORKSPACE_WRITE)
        {
            agent_contracts::ToolRisk::WorkspaceWrite
        } else {
            agent_contracts::ToolRisk::ReadOnly
        };
        let adapter = agent_capability_process::McpCapabilityAdapter::connect(
            decl.clone(),
            risk,
            agent_capability_process::DEFAULT_MCP_REQUEST_TIMEOUT,
            agent_capability_process::DEFAULT_MCP_MAX_FRAME_BYTES,
        )
        .await
        .map_err(|error| anyhow::anyhow!("MCP server '{}' discovery failed: {error}", decl.id))?;
        capability_registry
            .register(Arc::new(adapter))
            .map_err(|error| {
                anyhow::anyhow!("MCP capability '{}' registration failed: {error}", decl.id)
            })?;
        // The operator's explicit configuration IS the enabling act:
        // experimental-status capabilities register disabled (fail-closed),
        // and a configured server must be usable without a second manual
        // step.
        capability_registry
            .set_activation(&decl.id, agent_contracts::CapabilityActivation::Enabled)
            .await
            .map_err(|error| {
                anyhow::anyhow!("MCP capability '{}' could not be enabled: {error}", decl.id)
            })?;
    }
    let tools: Arc<dyn ToolDispatcher> = if capability_aware {
        let mut dispatcher = CapabilityAwareDispatcher::with_workspace(
            base_tools,
            capability_registry,
            // Capabilities that declare workspace/artifact permissions
            // receive confined handles into the same workspace the builtin
            // tools use.
            Some(Arc::new(workspace.clone())),
        )
        .with_host_policies(host_policies.clone());
        // E1: the plugin catalog backs on-demand skill body reads.
        if let Some(plugins) = plugins.as_ref() {
            dispatcher = dispatcher.with_plugin_registry(plugins.clone());
        }
        Arc::new(dispatcher)
    } else {
        base_tools
    };
    host.add_module(Arc::new(ToolModule::new(tools)))?;
    host.add_module(Arc::new(ApprovalModule::new(approval)))?;
    if let Some(journal) = journal {
        host.add_module(Arc::new(EventModule::new(journal)))?;
    }
    // Keep a handle for the runtime services: an artifact store means the
    // runtime's automatic safe-point writes have a real envelope store.
    // Without it the actor-side checkpoint writes fail "no checkpoint store
    // configured", durable resume checkpoints never land, and continuation
    // is fenced after every mutating turn.
    let artifact_store_for_services = artifact_store.clone();
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
    services = services.with_prompt_layout(prompt_layout);
    services = services.with_project_settlement(project_settlement);
    services = services.with_settlement_projection_diagnostics(settlement_projection_diagnostics);
    services = services.with_project_completion_opportunity(project_completion_opportunity);
    services = services.with_recovery_surface(recovery_surface);
    if let Some(digest) = provider_profile_digest_in {
        services.set_provider_profile_digest(digest);
    }
    services = services.with_defer_proof_refresh(defer_proof_refresh);
    services = services.with_shadow_context_frame(shadow_context_frame);
    if let Some(artifact_store) = artifact_store_for_services {
        services = services.with_artifact_workspace(artifact_store);
    }
    // A recipe table always installs the read-only domain resolver so a
    // model-facing repair can name an exact recipe on cold start. The switch
    // controls only Runtime's optional automatic execution of that recipe.
    // Enabling execution without a table remains a fail-closed boot error.
    match verification_recipes {
        Some(recipes) if !recipes.is_empty() => {
            // M17-B1: the host proof lane receives the same supervision
            // policy the caller gave the tool dispatcher — one decision,
            // both process lanes (F03).
            let runner = tool_runtime::RecipeProofRunner::new(workspace.clone(), recipes)
                .ok_or_else(|| anyhow::anyhow!("verification recipes register no host policy"))?
                .with_host_death_watchdog(host_death_watchdog);
            services = services.with_proof_verifier(Arc::new(HostProofVerifier::new(runner)));
            if project_proof_refresh {
                services = services.with_project_proof_refresh(true);
            }
        }
        Some(_) | None if project_proof_refresh => {
            return Err(anyhow::anyhow!(
                "project proof refresh requires host verification recipes"
            ));
        }
        _ => {}
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
    use agent_contracts::{
        ApprovalGate, ContextEngine, ContextIngress, ContextMaintenanceTrigger, ModelRequest,
        ModelTransport, ToolDispatcher,
    };
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
            provider_profile_digest: None,
            defer_proof_refresh: false,
            shadow_context_frame: false,
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
            // Test composition: no watchdog dispatch in this executable.
            host_death_watchdog: false,
            // Harness/eval compositions register no external capabilities by default.
            mcp_servers: Vec::new(),
            plugins: None,
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

    #[tokio::test]
    async fn empty_recipe_table_without_refresh_composes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(Vec::new()).unwrap());
        run_smoke(compose_config(workspace, Some(recipes), false)).await;
    }

    #[tokio::test]
    async fn empty_recipe_table_with_refresh_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(dir.path()).await.unwrap();
        let recipes = Arc::new(VerificationRecipes::new(Vec::new()).unwrap());
        let error = match compose(compose_config(workspace, Some(recipes), true)).await {
            Ok(_) => panic!("empty recipe table must not enable proof refresh"),
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
                cancel: agent_contracts::CancellationToken::new(),
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
    /// COST-4 (D02): the OPTIONAL maintenance transport OWNS the compactor —
    /// a folding pass goes to the maintenance model (which fails here), and
    /// the main model is never asked. The failed call still lands in the
    /// report as an Unknown-identity compaction (COST-1 accounting).
    #[tokio::test]
    async fn the_maintenance_transport_owns_the_compactor() {
        struct FailingMaintenance;
        #[async_trait::async_trait]
        impl ModelTransport for FailingMaintenance {
            fn capabilities(&self) -> agent_contracts::ModelCapabilities {
                agent_contracts::ModelCapabilities::default()
            }
            async fn complete(
                &self,
                _request: ModelRequest,
            ) -> agent_contracts::AgentResult<agent_contracts::ModelOutput> {
                Err(agent_contracts::AgentError::Model(
                    "maintenance transport down".into(),
                ))
            }
        }
        struct CountingMain {
            calls: std::sync::Arc<std::sync::Mutex<u32>>,
        }
        #[async_trait::async_trait]
        impl ModelTransport for CountingMain {
            fn capabilities(&self) -> agent_contracts::ModelCapabilities {
                agent_contracts::ModelCapabilities::default()
            }
            async fn complete(
                &self,
                _request: ModelRequest,
            ) -> agent_contracts::AgentResult<agent_contracts::ModelOutput> {
                *self.calls.lock().unwrap() += 1;
                Ok(agent_contracts::ModelOutput {
                    content: "main model should never compress".into(),
                    tool_calls: Vec::new(),
                    usage: agent_contracts::ModelUsage::default(),
                })
            }
        }

        let main_calls = std::sync::Arc::new(std::sync::Mutex::new(0u32));
        let main = Arc::new(CountingMain {
            calls: Arc::clone(&main_calls),
        });
        let engine = build_context_engine(
            ContextPolicy::Rolling,
            std::path::Path::new("unused-by-rolling"),
            Some(main),
            Some(Arc::new(FailingMaintenance)),
            &MaintenanceBudget::default(),
        )
        .await
        .unwrap();
        // Cross the rolling fold threshold (default: 9,000 summary tokens
        // with an 8,000 verbatim tail) so the next maintain runs the
        // compactor.
        for index in 0..40 {
            engine
                .ingest(ContextIngress::AssistantMessage {
                    content: format!("history record {index}: {}", "detail ".repeat(140)),
                })
                .await
                .unwrap();
        }
        let report = engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();

        let failed = report
            .compactions
            .first()
            .expect("the failed compactor call must be accounted");
        assert_eq!(
            failed.usage_identity,
            agent_contracts::UsageIdentity::Unknown,
            "the maintenance transport's failed call is accounted as unknown"
        );
        assert_eq!(
            *main_calls.lock().unwrap(),
            0,
            "the main model must never be asked to compress"
        );
    }
}

#[cfg(test)]
mod maintenance_budget_tests {
    use super::*;

    /// Env mutations are process-global; serialize them.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// COST-8: the product config reaches the engine's per-pass limits —
    /// a budget's count/token/backoff values become the rolling config, and
    /// a zero budget attaches no compactor at all (zero never sends).
    #[tokio::test]
    async fn the_maintenance_budget_flows_into_the_engine_config() {
        // Zero budget: the compactor is NOT attached — the model would
        // never be asked to compress, whatever the history pressure. The
        // model panics if the budget ever leaked a call through.
        struct ZeroBudgetModel;
        #[async_trait::async_trait]
        impl ModelTransport for ZeroBudgetModel {
            fn capabilities(&self) -> agent_contracts::ModelCapabilities {
                agent_contracts::ModelCapabilities::default()
            }
            async fn complete(
                &self,
                _request: agent_contracts::ModelRequest,
            ) -> AgentResult<agent_contracts::ModelOutput> {
                panic!("a zero maintenance budget must never reach the model");
            }
        }
        // The budget description is checkable, one line, key-free.
        let budget = MaintenanceBudget {
            max_calls_per_maintain: 1,
            max_tokens_per_maintain: 100,
            compact_failure_backoff_maintains: 2,
        };
        assert_eq!(
            budget.describe(),
            "maintenance budget: <= 1 compactor call(s) and <= 100 token(s) per maintain pass; failed folds back off 2 pass(es)"
        );

        let zero = MaintenanceBudget {
            max_calls_per_maintain: 0,
            ..budget
        };
        assert!(!zero.allows_calls());
        let engine = build_context_engine(
            ContextPolicy::Rolling,
            std::path::Path::new("unused-by-rolling"),
            Some(Arc::new(ZeroBudgetModel)),
            None,
            &zero,
        )
        .await
        .unwrap();
        // The engine still works as a plain rolling engine without a
        // compactor.
        engine
            .ingest(agent_contracts::ContextIngress::AssistantMessage {
                content: "plain record".into(),
            })
            .await
            .unwrap();
        engine
            .maintain(agent_contracts::ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
    }

    /// COST-8: budget env variables parse strictly — unset keeps defaults,
    /// an invalid value is a startup error, never a silent fallback.
    #[test]
    fn maintenance_budget_env_parses_strictly() {
        let _guard = ENV_LOCK.lock().unwrap();
        let keys = [
            "MAINTENANCE_MAX_CALLS_PER_MAINTAIN",
            "MAINTENANCE_MAX_TOKENS_PER_MAINTAIN",
            "MAINTENANCE_COMPACT_FAILURE_BACKOFF",
        ];
        let mut saved = Vec::new();
        for key in keys {
            saved.push((key, std::env::var(key).ok()));
            unsafe { std::env::remove_var(key) };
        }
        // Unset: the defaults (and the default is NOT a claimed cap).
        let budget = maintenance_budget_from_env().unwrap();
        assert_eq!(budget, MaintenanceBudget::default());
        assert!(budget.describe().contains("unbounded"));

        // Explicit values parse.
        unsafe { std::env::set_var("MAINTENANCE_MAX_CALLS_PER_MAINTAIN", "2") };
        unsafe { std::env::set_var("MAINTENANCE_MAX_TOKENS_PER_MAINTAIN", "5000") };
        unsafe { std::env::set_var("MAINTENANCE_COMPACT_FAILURE_BACKOFF", "1") };
        let budget = maintenance_budget_from_env().unwrap();
        assert_eq!(budget.max_calls_per_maintain, 2);
        assert_eq!(budget.max_tokens_per_maintain, 5000);
        assert_eq!(budget.compact_failure_backoff_maintains, 1);

        // Invalid values fail closed.
        unsafe { std::env::set_var("MAINTENANCE_MAX_CALLS_PER_MAINTAIN", "two") };
        assert!(maintenance_budget_from_env().is_err());
        unsafe { std::env::set_var("MAINTENANCE_MAX_CALLS_PER_MAINTAIN", "2") };
        unsafe { std::env::set_var("MAINTENANCE_MAX_TOKENS_PER_MAINTAIN", "-1") };
        assert!(maintenance_budget_from_env().is_err());

        for (key, value) in saved {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
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
            ModelSelection::Provider(_, _)
        ));
    }

    #[test]
    fn invalid_provider_values_are_startup_errors_never_silent_defaults() {
        let _env = ENV_LOCK.lock().unwrap();
        let keys = [
            "OPENAI_API_KEY",
            "AGENT_DEMO",
            "OPENAI_API_PROTOCOL",
            "OPENAI_CONTEXT_WINDOW",
            "OPENAI_MAX_OUTPUT_TOKENS",
            "OPENAI_TEMPERATURE",
            "OPENAI_PROMPT_CACHE_MODE",
            "OPENAI_RESPONSES_REASONING_EFFORT",
        ];
        // Protocol: the historical silent path turned any garbage into Auto.
        let _guard = EnvGuard::new(&keys);
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test") };
        unsafe { std::env::set_var("OPENAI_API_PROTOCOL", "grapefruit") };
        let error = try_model_from_env().unwrap_err().to_string();
        assert!(error.contains("OPENAI_API_PROTOCOL"), "{error}");

        // Context window: no silent 128k fallback on a typo.
        unsafe { std::env::remove_var("OPENAI_API_PROTOCOL") };
        unsafe { std::env::set_var("OPENAI_CONTEXT_WINDOW", "12o800") };
        let error = try_model_from_env().unwrap_err().to_string();
        assert!(error.contains("OPENAI_CONTEXT_WINDOW"), "{error}");

        // Temperature: out of range is refused, in range is pinned.
        unsafe { std::env::remove_var("OPENAI_CONTEXT_WINDOW") };
        unsafe { std::env::set_var("OPENAI_TEMPERATURE", "9.5") };
        let error = try_model_from_env().unwrap_err().to_string();
        assert!(error.contains("OPENAI_TEMPERATURE"), "{error}");
        unsafe { std::env::set_var("OPENAI_TEMPERATURE", "0.2") };
        match try_model_from_env().unwrap() {
            ModelSelection::Provider(_, profile) => {
                assert_eq!(
                    profile.sampling,
                    provider_openai::SamplingPolicy::Temperature(0.2)
                );
                assert!(profile.banner().contains("temperature=0.2"));
                assert!(profile.banner().contains("digest="));
            }
            other => panic!("expected a provider selection, got {other:?}"),
        }
        unsafe { std::env::set_var("OPENAI_RESPONSES_REASONING_EFFORT", "bogus") };
        assert!(
            try_model_from_env()
                .unwrap_err()
                .to_string()
                .contains("OPENAI_RESPONSES_REASONING_EFFORT")
        );
        unsafe { std::env::set_var("OPENAI_RESPONSES_REASONING_EFFORT", "none") };
        assert!(
            try_model_from_env()
                .unwrap_err()
                .to_string()
                .contains("requires OPENAI_API_PROTOCOL=responses")
        );
        unsafe { std::env::set_var("OPENAI_API_PROTOCOL", "responses") };
        let ModelSelection::Provider(_, profile) = try_model_from_env().unwrap() else {
            panic!("provider expected");
        };
        assert_eq!(
            profile.responses_reasoning_effort,
            ResponsesReasoningEffort::None
        );
    }

    #[test]
    fn the_profile_digest_separates_sampling_operating_points() {
        let base = ProviderProfile {
            base_url: "https://example.com/v1".into(),
            model: "m".into(),
            protocol: "responses",
            context_window: 128_000,
            max_output_tokens: 4096,
            sampling: provider_openai::SamplingPolicy::ProviderDefault,
            prompt_cache_mode: OpenAiPromptCacheMode::ProviderDefault,
            responses_reasoning_effort: ResponsesReasoningEffort::ProviderDefault,
        };
        let pinned = ProviderProfile {
            sampling: provider_openai::SamplingPolicy::Temperature(0.0),
            ..base.clone()
        };
        assert_ne!(base.digest(), pinned.digest());
        assert_eq!(base.digest(), base.clone().digest());
        // The digest never carries the key material because the profile
        // struct has no key field.
        assert!(!base.digest().is_empty());
        let explicit = ProviderProfile {
            prompt_cache_mode: OpenAiPromptCacheMode::ResponsesExplicit,
            ..base.clone()
        };
        assert_ne!(base.digest(), explicit.digest());
        assert!(
            explicit
                .banner()
                .contains("prompt_cache=responses_explicit")
        );
        let non_thinking = ProviderProfile {
            responses_reasoning_effort: ResponsesReasoningEffort::None,
            ..base.clone()
        };
        assert_ne!(base.digest(), non_thinking.digest());
        assert!(non_thinking.banner().contains("responses_reasoning=none"));
    }

    #[test]
    fn explicit_cache_configuration_is_checked_before_runtime_construction() {
        let _env = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new(&[
            "OPENAI_API_KEY",
            "AGENT_DEMO",
            "OPENAI_API_PROTOCOL",
            "OPENAI_PROMPT_CACHE_MODE",
            "OPENAI_TEMPERATURE",
            "OPENAI_CONTEXT_WINDOW",
            "OPENAI_MAX_OUTPUT_TOKENS",
        ]);
        unsafe { std::env::set_var("OPENAI_API_KEY", "sk-test") };
        unsafe { std::env::set_var("OPENAI_PROMPT_CACHE_MODE", "typo") };
        assert!(
            try_model_from_env()
                .unwrap_err()
                .to_string()
                .contains("OPENAI_PROMPT_CACHE_MODE")
        );
        unsafe { std::env::set_var("OPENAI_PROMPT_CACHE_MODE", "responses_explicit") };
        for protocol in ["auto", "chat"] {
            unsafe { std::env::set_var("OPENAI_API_PROTOCOL", protocol) };
            assert!(
                try_model_from_env()
                    .unwrap_err()
                    .to_string()
                    .contains("requires OPENAI_API_PROTOCOL=responses")
            );
        }
        unsafe { std::env::set_var("OPENAI_API_PROTOCOL", "responses") };
        let ModelSelection::Provider(_, profile) = try_model_from_env().unwrap() else {
            panic!("provider expected");
        };
        assert_eq!(
            profile.prompt_cache_mode,
            OpenAiPromptCacheMode::ResponsesExplicit
        );
    }
}
