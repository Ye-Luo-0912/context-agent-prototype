use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AgentError, AgentResult, CancellationToken, ContextAction, ContextItemId, ContextKind,
    ContextScope, EffectId, RunId, TaskId, ToolOperationIdentity, TurnId,
};

/// Hard cap on the model-facing `model_content` of a tool result (chars).
/// Producers may choose smaller limits; none may make the model-facing
/// result larger. Matches the reference context engine's per-item ceiling.
pub const MAX_TOOL_MODEL_CONTENT_CHARS: usize = 16_000;

/// Hard cap on the `summary` field of a tool result (chars).
pub const MAX_TOOL_SUMMARY_CHARS: usize = 2_000;

/// Hard cap on the serialized size of the `metadata` field of a tool
/// result (bytes of JSON). Oversized metadata is replaced by a bounded
/// marker that keeps the decoded total honest.
pub const MAX_TOOL_METADATA_BYTES: usize = 8_000;

/// Decoded-total cap: the model-facing view (`summary` + `model_content` +
/// serialized `metadata`) must stay under this many chars even when each
/// field individually fits. The broker trims `model_content` first.
pub const MAX_TOOL_OUTPUT_TOTAL_CHARS: usize = 24_000;

/// `context.search` 的 limit 上限，在执行期强制（JSON schema 只负责声明
/// 同一个最大值；执行期才是权威）。
pub const CONTEXT_SEARCH_MAX_LIMIT: usize = 50;

/// `context.search` 自由文本查询的长度上限，在执行期强制：超长查询在
/// 到达引擎前按字符截断，模型永远无法用巨型查询字符串冲刷检索路径。
pub const CONTEXT_SEARCH_MAX_QUERY_CHARS: usize = 256;

/// A trusted output broker: bounds every model-facing field of a tool
/// output and spills oversized content to an artifact once, returning a
/// bounded preview plus a reference. The kernel applies it before a
/// `ToolOutcome` reaches the actor, so a producer that did not spill cannot
/// lose the truncated middle and no field can blow past its cap.
///
/// `budget` is the declaring tool's own model-content cap (`ToolSpec.
/// output_budget`): `Some(n)` bounds `model_content` at `n` chars (never
/// above the global cap), `None` falls back to the global cap. Producers
/// may choose smaller limits; none may make the model-facing result larger.
#[async_trait]
pub trait OutputBroker: Send + Sync {
    /// Bound `output` under the run's artifact store. `run_id` selects the
    /// artifact directory so spills land with the run's other artifacts.
    async fn bound(&self, run_id: RunId, budget: Option<usize>, output: ToolOutput) -> ToolOutput;
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub enum ToolRisk {
    #[default]
    ReadOnly,
    WorkspaceWrite,
    ProcessExecution,
}

impl ToolRisk {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::ProcessExecution => "process_execution",
        }
    }
}

/// The normalized side effect one tool call intends to perform, derived from
/// the validated arguments — never from the tool's self-declared risk
/// alone. Approval/policy matches this concrete intent, and the executor
/// proves the actual effect fits it before commit. It is a conservative
/// upper bound by design: a workspace write carries its target path and a
/// byte estimate of the content, a process run its lexical command prefix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectIntent {
    /// A read-only call; nothing to authorize.
    ReadOnly,
    /// A workspace write: the workspace-relative target path and a
    /// conservative byte estimate of the content being written.
    WorkspaceWrite { path: String, content_bytes: u64 },
    /// A process run: the lexical command (whitespace-separated tokens).
    ProcessRun { command: String },
}

impl EffectIntent {
    /// The effect class the intent belongs to — the bridge to the legacy
    /// `ToolRisk` grant vocabulary until grants move to typed permissions.
    pub fn risk(&self) -> ToolRisk {
        match self {
            Self::ReadOnly => ToolRisk::ReadOnly,
            Self::WorkspaceWrite { .. } => ToolRisk::WorkspaceWrite,
            Self::ProcessRun { .. } => ToolRisk::ProcessExecution,
        }
    }
}

/// Derive the conservative `EffectIntent` upper bound from validated tool
/// arguments. This is the shared normalization both the legacy
/// standing-grant gate and the v2 lease/`AuthorityGate` path use:
/// approval matches the concrete intent, never the tool name. Fail-closed:
/// a missing argument yields the empty/zero bound, which can never match a
/// grant (an empty path has no prefix, an empty command has no tokens).
pub fn derive_effect_intent(call: &ToolCall, spec: &ToolSpec) -> EffectIntent {
    match spec.risk {
        ToolRisk::ReadOnly => EffectIntent::ReadOnly,
        ToolRisk::WorkspaceWrite => {
            let path = call
                .arguments
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let content_bytes = call
                .arguments
                .get("content")
                .or_else(|| call.arguments.get("new"))
                .and_then(|value| value.as_str())
                .map(|content| content.len() as u64)
                .unwrap_or(0);
            EffectIntent::WorkspaceWrite {
                path,
                content_bytes,
            }
        }
        ToolRisk::ProcessExecution => {
            let command = call
                .arguments
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            EffectIntent::ProcessRun { command }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: ToolRisk,
    /// Declared model-content cap for this tool's results, in chars. A
    /// tool that produces verbose output declares a smaller budget so the
    /// broker spills sooner; `None` uses the global
    /// `MAX_TOOL_MODEL_CONTENT_CHARS`. Execution is authoritative — the
    /// broker enforces `min(budget, global)` regardless of the declared
    /// value, so a declaration can never exceed the hard cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_budget: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutput {
    pub call_id: String,
    pub tool_name: String,
    pub ok: bool,
    pub summary: String,
    /// Bounded content intended for the next model turn.
    pub model_content: String,
    /// Raw/large output should live here instead of in the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

impl ToolOutput {
    /// 工具在 `metadata.path` 上盖的工作区相对路径。ingest 用它做结构化
    /// 身份；`fs.read` 的 model_content 是带行号的片段，没有路径。
    pub fn file_path(&self) -> Option<&str> {
        self.metadata
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|path| !path.is_empty())
    }

    /// 工具在 `metadata.revision` 上盖的内容摘要。
    pub fn file_revision(&self) -> Option<&str> {
        self.metadata
            .get("revision")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|revision| !revision.is_empty())
    }

    /// Command or argv the producer stamped, used to identity a failed
    /// operation without collapsing every `shell.exec` into one slot.
    pub fn operation_target(&self) -> Option<&str> {
        if let Some(path) = self.file_path() {
            return Some(path);
        }
        if let Some(command) = self
            .metadata
            .get("command")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|command| !command.is_empty())
        {
            return Some(command);
        }
        self.metadata
            .get("argv")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|argv| !argv.is_empty())
    }

    /// Explicit verification intent, never inferred from the tool name.
    pub fn is_verification(&self) -> bool {
        if self
            .metadata
            .get("verification")
            .and_then(|value| value.as_bool())
            == Some(true)
        {
            return true;
        }
        if self.metadata.get("intent").and_then(|value| value.as_str()) == Some("verify") {
            return true;
        }
        self.operation_target()
            .is_some_and(command_looks_like_verification)
    }

    /// 中轮 WorkingSetSignal 文本：路径单独一行，后面才是正文。
    pub fn working_set_signal_text(&self) -> String {
        match self.file_path() {
            Some(path) => format!("{path}\n{}", self.model_content),
            None => self.model_content.clone(),
        }
    }

    /// Trusted failure class. Core writes it under `metadata._runtime`;
    /// top-level `failure_class` is accepted only as a producer hint or
    /// legacy trace.
    pub fn failure_class(&self) -> Option<ToolFailureClass> {
        self.metadata
            .get(RUNTIME_METADATA_KEY)
            .and_then(|value| value.get(TOOL_FAILURE_CLASS_KEY))
            .and_then(|value| value.as_str())
            .and_then(ToolFailureClass::parse)
            .or_else(|| {
                self.metadata
                    .get(TOOL_FAILURE_CLASS_KEY)
                    .and_then(|value| value.as_str())
                    .and_then(ToolFailureClass::parse)
            })
    }

    /// Successful semantic observations may heat the working set.
    /// Failed execution results stay on the TurnFrame and must not.
    pub fn heats_working_set(&self) -> bool {
        self.ok && self.failure_class().is_none()
    }
}

fn command_looks_like_verification(command: &str) -> bool {
    let text = command.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "cargo test",
        "cargo check",
        "cargo clippy",
        "pytest",
        "dotnet test",
        "go test",
        "mvn test",
        "gradle test",
        "npm test",
        "rustc ",
        "rustc.exe",
    ];
    NEEDLES.iter().any(|needle| text.contains(needle))
}

/// Metadata key for [`ToolFailureClass`]. Producers must not set `retryable`.
pub const TOOL_FAILURE_CLASS_KEY: &str = "failure_class";
/// Optional bounded corrective fact for the next model turn.
pub const TOOL_RECOVERY_HINT_KEY: &str = "recovery_hint";
/// Hard cap on `recovery_hint` characters.
pub const TOOL_RECOVERY_HINT_MAX_CHARS: usize = 256;
/// Core-owned diagnosis object. Producers cannot write this key; the output
/// authority strips it and writes the trusted copy (`TOOL-ERROR-01`).
pub const RUNTIME_METADATA_KEY: &str = "_runtime";
const RESERVED_RUNTIME_METADATA_KEYS: &[&str] = &[
    RUNTIME_METADATA_KEY,
    TOOL_FAILURE_CLASS_KEY,
    TOOL_RECOVERY_HINT_KEY,
    "retryable",
];

/// Trusted, model-facing cause of a tool refusal or failed execution.
///
/// The kernel/runtime projects this class. A producer must not mark its own
/// failure retryable or widen authority (`TOOL-ERROR-01`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureClass {
    ShellDialectMismatch,
    CommandUnavailable,
    MissingProjectMarker,
    StaleRevision,
    NoExactMatch,
    AmbiguousMatch,
    NoSearchMatch,
    ProcessExit,
    VerificationFailure,
    Timeout,
    Cancellation,
    HiddenPath,
    PathNotFound,
    InvalidRequest,
    Io,
}

impl ToolFailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ShellDialectMismatch => "shell_dialect_mismatch",
            Self::CommandUnavailable => "command_unavailable",
            Self::MissingProjectMarker => "missing_project_marker",
            Self::StaleRevision => "stale_revision",
            Self::NoExactMatch => "no_exact_match",
            Self::AmbiguousMatch => "ambiguous_match",
            Self::NoSearchMatch => "no_search_match",
            Self::ProcessExit => "process_exit",
            Self::VerificationFailure => "verification_failure",
            Self::Timeout => "timeout",
            Self::Cancellation => "cancellation",
            Self::HiddenPath => "hidden_path",
            Self::PathNotFound => "path_not_found",
            Self::InvalidRequest => "invalid_request",
            Self::Io => "io",
        }
    }

    pub const fn default_recovery_hint(self) -> &'static str {
        match self {
            Self::ShellDialectMismatch => {
                "Use the selected shell dialect named in the shell.exec schema."
            }
            Self::CommandUnavailable => {
                "The command is not available in this shell; pick another command or process.run."
            }
            Self::MissingProjectMarker => {
                "The expected project manifest is absent; do not invent one."
            }
            Self::StaleRevision => {
                "Re-read the file and retry with the current revision. Matching stays exact."
            }
            Self::NoExactMatch => {
                "Re-read and supply an exact unique anchor. Matching stays exact."
            }
            Self::AmbiguousMatch => {
                "Supply occurrence or a unique exact anchor. Matching stays exact."
            }
            Self::NoSearchMatch => "Broaden or change the query; do not invent missing files.",
            Self::ProcessExit => "Inspect the bounded output and fix the command or code.",
            Self::VerificationFailure => {
                "Hidden verification failed; inspect the remaining assertions."
            }
            Self::Timeout => "Narrow the command or raise the timeout within the schema cap.",
            Self::Cancellation => "The operation was cancelled; do not assume it finished.",
            Self::HiddenPath => "Use artifact.read or git.* instead of ordinary file tools.",
            Self::PathNotFound => {
                "Use a path that exists in the parent listing; do not invent manifests."
            }
            Self::InvalidRequest => "Fix the arguments against the tool schema.",
            Self::Io => "Retry only after checking the path and workspace confinement.",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "shell_dialect_mismatch" => Self::ShellDialectMismatch,
            "command_unavailable" => Self::CommandUnavailable,
            "missing_project_marker" => Self::MissingProjectMarker,
            "stale_revision" => Self::StaleRevision,
            "no_exact_match" => Self::NoExactMatch,
            "ambiguous_match" => Self::AmbiguousMatch,
            "no_search_match" => Self::NoSearchMatch,
            "process_exit" => Self::ProcessExit,
            "verification_failure" => Self::VerificationFailure,
            "timeout" => Self::Timeout,
            "cancellation" => Self::Cancellation,
            "hidden_path" => Self::HiddenPath,
            "path_not_found" => Self::PathNotFound,
            "invalid_request" => Self::InvalidRequest,
            "io" => Self::Io,
            _ => return None,
        })
    }
}

/// Insert `failure_class` and strip any producer-supplied `retryable` flag.
pub fn attach_failure_class(metadata: &mut Value, class: ToolFailureClass) {
    let object = match metadata {
        Value::Object(map) => map,
        _ => {
            *metadata = serde_json::json!({});
            metadata
                .as_object_mut()
                .expect("empty object is always an object")
        }
    };
    object.insert(
        TOOL_FAILURE_CLASS_KEY.into(),
        Value::String(class.as_str().into()),
    );
    object.remove("retryable");
    object
        .entry(TOOL_RECOVERY_HINT_KEY.to_string())
        .or_insert_with(|| Value::String(class.default_recovery_hint().into()));
    if let Some(Value::String(hint)) = object.get_mut(TOOL_RECOVERY_HINT_KEY) {
        let bounded: String = hint.chars().take(TOOL_RECOVERY_HINT_MAX_CHARS).collect();
        *hint = bounded;
    }
}

/// Bounded failed tool result with a trusted class. `ok` is always false.
pub fn tool_failure_output(
    call_id: impl Into<String>,
    tool_name: impl Into<String>,
    class: ToolFailureClass,
    summary: impl Into<String>,
    model_content: impl Into<String>,
    mut metadata: Value,
) -> ToolOutput {
    attach_failure_class(&mut metadata, class);
    ToolOutput {
        call_id: call_id.into(),
        tool_name: tool_name.into(),
        ok: false,
        summary: summary.into(),
        model_content: model_content.into(),
        artifact_ref: None,
        metadata,
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeDiagnosis {
    pub class: ToolFailureClass,
    pub hint: String,
}

/// Strip producer `_runtime` / `failure_class` / `recovery_hint` / `retryable`
/// and return the Core-owned diagnosis to write back after output bounding.
pub fn take_runtime_diagnosis(output: &mut ToolOutput) -> Option<RuntimeDiagnosis> {
    // `_runtime` is Core-owned: ignore any producer copy. First-party tools
    // pass a typed class via the top-level producer hint.
    let producer_class = output
        .metadata
        .get(TOOL_FAILURE_CLASS_KEY)
        .and_then(|value| value.as_str())
        .and_then(ToolFailureClass::parse);
    let producer_hint = output
        .metadata
        .get(TOOL_RECOVERY_HINT_KEY)
        .and_then(|value| value.as_str())
        .map(|hint| {
            hint.chars()
                .take(TOOL_RECOVERY_HINT_MAX_CHARS)
                .collect::<String>()
        })
        .filter(|hint| !hint.is_empty());
    strip_reserved_runtime_metadata(&mut output.metadata);
    let class = producer_class.or_else(|| {
        if output.ok {
            None
        } else {
            Some(failure_class_from_message(&format!(
                "{}\n{}",
                output.summary, output.model_content
            )))
        }
    })?;
    Some(RuntimeDiagnosis {
        hint: producer_hint.unwrap_or_else(|| class.default_recovery_hint().to_string()),
        class,
    })
}

/// Write trusted `_runtime` and a model-visible header. Call after the
/// output broker has bounded producer fields.
pub fn apply_runtime_diagnosis(output: &mut ToolOutput, diagnosis: Option<RuntimeDiagnosis>) {
    let diagnosis = match diagnosis {
        Some(diagnosis) => diagnosis,
        None if !output.ok => {
            let class = failure_class_from_message(&format!(
                "{}\n{}",
                output.summary, output.model_content
            ));
            RuntimeDiagnosis {
                hint: class.default_recovery_hint().to_string(),
                class,
            }
        }
        None => return,
    };
    write_runtime_metadata(&mut output.metadata, diagnosis.class, &diagnosis.hint);
    prepend_runtime_failure_header(&mut output.model_content, diagnosis.class, &diagnosis.hint);
}

fn strip_reserved_runtime_metadata(metadata: &mut Value) {
    let Some(object) = metadata.as_object_mut() else {
        return;
    };
    for key in RESERVED_RUNTIME_METADATA_KEYS {
        object.remove(*key);
    }
}

fn write_runtime_metadata(metadata: &mut Value, class: ToolFailureClass, hint: &str) {
    let object = match metadata {
        Value::Object(map) => map,
        _ => {
            *metadata = serde_json::json!({});
            metadata
                .as_object_mut()
                .expect("empty object is always an object")
        }
    };
    let hint: String = hint.chars().take(TOOL_RECOVERY_HINT_MAX_CHARS).collect();
    object.insert(
        RUNTIME_METADATA_KEY.into(),
        serde_json::json!({
            TOOL_FAILURE_CLASS_KEY: class.as_str(),
            TOOL_RECOVERY_HINT_KEY: hint,
        }),
    );
}

fn prepend_runtime_failure_header(content: &mut String, class: ToolFailureClass, hint: &str) {
    if content.starts_with("runtime_failure:") {
        return;
    }
    let hint: String = hint.chars().take(TOOL_RECOVERY_HINT_MAX_CHARS).collect();
    let header = format!(
        "runtime_failure:\nclass={}\nhint={}\n\n",
        class.as_str(),
        hint
    );
    content.insert_str(0, &header);
}

/// Classify an [`AgentError`] that escaped as `Err` instead of a typed tool
/// result. Prefer returning [`tool_failure_output`] from tools so recovery
/// facts survive; this path is the kernel's last projection.
pub fn failure_class_from_agent_error(error: &crate::AgentError) -> ToolFailureClass {
    use crate::AgentError;
    match error {
        AgentError::Cancelled => ToolFailureClass::Cancellation,
        AgentError::InvalidRequest(message) => failure_class_from_message(message),
        AgentError::Io(_) => ToolFailureClass::Io,
        AgentError::Tool(message) => failure_class_from_message(message),
        _ => failure_class_from_message(&error.to_string()),
    }
}

/// True when a workspace/tool I/O string is a missing path.
///
/// Windows confined opens often display as `NTSTATUS 0xc0000034` /
/// `0xc000003a` without the words "not found".
pub fn message_looks_like_not_found(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("not found")
        || m.contains("no such file")
        || m.contains("cannot find")
        || m.contains("os error 2")
        || m.contains("os error 3")
        || m.contains("ntstatus 0xc0000034")
        || m.contains("ntstatus 0xc000003a")
}

/// Best-effort classification from a free-text error. Used when a tool or
/// the kernel only has a string.
pub fn failure_class_from_message(message: &str) -> ToolFailureClass {
    let m = message.to_ascii_lowercase();
    if m.contains("cancelled") {
        ToolFailureClass::Cancellation
    } else if m.contains("timed out") || m.contains("timeout") {
        ToolFailureClass::Timeout
    } else if m.contains("base_revision") || m.contains("stale_revision") {
        ToolFailureClass::StaleRevision
    } else if m.contains("disambiguate") || m.contains("ambiguous") {
        ToolFailureClass::AmbiguousMatch
    } else if m.contains("appears 0") || m.contains("no_exact_match") || m.contains("no exact") {
        ToolFailureClass::NoExactMatch
    } else if m.contains("hidden_path") || (m.contains(".focus-agent") && m.contains("not allowed"))
    {
        ToolFailureClass::HiddenPath
    } else if message_looks_like_not_found(message) {
        ToolFailureClass::PathNotFound
    } else if m.contains("i/o error") || m.contains("io error") {
        ToolFailureClass::Io
    } else {
        ToolFailureClass::InvalidRequest
    }
}

/// What happens to a tool result after the turn: whether it becomes a new
/// long-term observation or stays transient.
///
/// Context retrieval (`context.search` / `context.inspect` / `context.fetch`)
/// is a transient store read: it must make evidence visible to the current
/// turn without duplicating it under a new observation id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolResultDisposition {
    /// Persist the result as a new `ToolObservation` (default).
    #[default]
    PersistObservation,
    /// The result is visible to the current turn only; it is not persisted
    /// as a new observation. Access stamps (last_access, GC epoch) are still
    /// recorded by the engine where the read happened.
    TransientNoPersist,
    /// The result is visible to the current turn only and is not persisted
    /// as a new observation — but the referenced item itself receives a
    /// lifecycle/access event (`context.admit`: the item re-enters the
    /// working set under its original id, one transition). Persisting the
    /// directive result would duplicate the admitted item under a new id,
    /// so the event *is* the record.
    AccessEventOnly,
}

/// Core-issued recovery identity attached only to a side-effecting dispatch.
///
/// This is evidence metadata, not an authority grant: committing still
/// requires the matching Core-held lease and current authority epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEffectContext {
    pub identity: ToolOperationIdentity,
    pub effect_id: EffectId,
}

impl OperationEffectContext {
    pub fn validate(&self) -> Result<(), String> {
        self.identity.validate()?;
        if self.effect_id.0.is_nil() {
            return Err("operation effect context contains a nil effect id".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionRequest {
    pub run_id: RunId,
    pub call: ToolCall,
    /// Stable identity for a side-effecting operation, issued and persisted
    /// by Core before dispatch. Legacy/read-only calls carry `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_context: Option<OperationEffectContext>,
    /// Cooperative cancellation handle for this execution (kill long-running
    /// processes, abort expensive searches). Not serialized.
    #[serde(skip)]
    pub cancel: CancellationToken,
}

impl ToolExecutionRequest {
    /// Validate all duplicated request identity before a dispatcher trusts it
    /// as recovery evidence. Tool risk remains policy-owned and is therefore
    /// intentionally not inferred here.
    pub fn validate(&self) -> Result<(), String> {
        let Some(context) = &self.effect_context else {
            return Ok(());
        };
        context.validate()?;
        if context.identity.run_id != self.run_id
            || context.identity.call_id != self.call.id
            || context.identity.tool_name != self.call.name
            || context.identity.argument_digest
                != crate::ArgumentDigest::from_json(&self.call.arguments)
        {
            return Err("operation effect context does not match the tool request".into());
        }
        Ok(())
    }
}

/// A side effect a tool prepared but did not yet apply.
///
/// The tool's *computation* (reading, diffing, staging a temp file) happens
/// inside execution; the *side-effect commit* is owned by the runtime. The
/// actor validates the operation against the generation fence and only then
/// calls `commit`; a stale operation (cancelled or superseded) must call
/// `rollback` so the staged effect never lands. This is what makes tool
/// cancellation safe: the actor can stop a stale mutation before it touches
/// the filesystem, the git index, an outbox or an external API.
#[async_trait::async_trait]
pub trait Effect: Send + Sync {
    /// Human-readable description for events and logs.
    fn describe(&self) -> String;
    /// Apply the prepared effect (atomic rename, outbox send, ...) and
    /// return the ACI v2 receipt: what happened to the world, how durably
    /// it is recorded, and the evidence reference. `NotApplied` leaves the
    /// world unchanged; `Applied` with `DurabilityFailed` means something
    /// landed but the operation is not durably complete (for example, a
    /// journal failure or a partial sequential composite) — the runtime
    /// must treat that as a degraded/recovery state, never as "nothing
    /// happened"; `Unknown` is for remote operations whose applied state
    /// can never be learned back.
    async fn commit(self: Box<Self>) -> EffectReceipt;
    /// Undo the preparation: the effect must not land.
    async fn rollback(self: Box<Self>, reason: &str);
}

/// How durably an applied effect is recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectDurability {
    /// Fully recorded; the world and the journal agree.
    Durable,
    /// The effect landed but the operation cannot be treated as durably
    /// complete. This includes a failed journal write and a sequential
    /// composite that stopped after an earlier sub-effect had already
    /// landed. Recovery is required.
    DurabilityFailed(String),
}

/// The outcome of committing a staged effect (ACI v2, compatibility order
/// step 5): what happened to the world, how durably it is recorded, and
/// the evidence reference. Single-effect `EffectCommitError` semantics are
/// preserved: `NotApplied` and `AppliedButDurabilityFailed` map one-to-one.
/// A sequential composite may also synthesize `DurabilityFailed` when it
/// cannot truthfully report `NotApplied` after an earlier child landed.
/// The receipt carries those states in one typed, serializable,
/// evidence-bearing shape.
/// Errors travel as message strings (receipts are result envelopes for
/// events and logs, not error objects).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EffectReceipt {
    /// The effect did not land; there is nothing to recover.
    NotApplied { error: String },
    /// The effect landed. `evidence` is a stable change id / reference for
    /// review or recovery (e.g. the workspace mutation transaction id).
    Applied {
        durability: EffectDurability,
        evidence: Option<String>,
    },
    /// The applied state is unknowable (a remote operation whose result
    /// never returned). Never blindly retried without an idempotency key.
    Unknown { error: String },
}

impl EffectReceipt {
    /// A short human-readable summary for logs and model-facing messages.
    pub fn summary(&self) -> String {
        match self {
            Self::NotApplied { error } => format!("effect not applied: {error}"),
            Self::Applied {
                durability: EffectDurability::Durable,
                ..
            } => "effect applied".to_string(),
            Self::Applied {
                durability: EffectDurability::DurabilityFailed(error),
                ..
            } => format!("effect applied but recovery is required: {error}"),
            Self::Unknown { error } => format!("effect applied state unknown: {error}"),
        }
    }
}

impl From<EffectCommitError> for EffectReceipt {
    fn from(error: EffectCommitError) -> Self {
        match error {
            EffectCommitError::NotApplied(error) => Self::NotApplied {
                error: error.to_string(),
            },
            EffectCommitError::AppliedButDurabilityFailed(error) => Self::Applied {
                durability: EffectDurability::DurabilityFailed(error.to_string()),
                evidence: None,
            },
        }
    }
}

/// Several trusted, already-prepared effects committed in order as one
/// operation. Multi-file builtin edits use this today. Process wire effects
/// remain fail-closed until their actual intent can be proved against the
/// invocation lease; if that path is later re-enabled it may reuse this
/// composite only after every child effect is safely staged.
/// Each sub-effect is itself atomic; a mid-list failure stops the rest and
/// rolls back every unattempted preparation. Effects already committed stay
/// committed (they are separate atomic operations, not one transaction), so
/// the aggregate reports `Applied` plus `DurabilityFailed` whenever a later
/// `NotApplied`/`Unknown` receipt follows an earlier application. Evidence
/// references are aggregated under a hard bound for recovery.
#[async_trait::async_trait]
impl Effect for Vec<Box<dyn Effect>> {
    fn describe(&self) -> String {
        format!("composite of {} staged effects", self.len())
    }

    async fn commit(self: Box<Self>) -> EffectReceipt {
        let mut effects = (*self).into_iter();
        let mut evidence = CompositeEvidence::default();
        let mut applied_count = 0usize;
        while let Some(effect) = effects.next() {
            match effect.commit().await {
                EffectReceipt::Applied {
                    durability: EffectDurability::Durable,
                    evidence: Some(id),
                } => {
                    applied_count += 1;
                    evidence.push(id);
                }
                EffectReceipt::Applied {
                    durability: EffectDurability::Durable,
                    evidence: None,
                } => {
                    applied_count += 1;
                }
                EffectReceipt::Applied {
                    durability: EffectDurability::DurabilityFailed(error),
                    evidence: own,
                } => {
                    evidence.push_optional(own);
                    rollback_remaining(
                        effects,
                        "composite commit stopped after an applied sub-effect required recovery",
                    )
                    .await;
                    return EffectReceipt::Applied {
                        durability: EffectDurability::DurabilityFailed(error),
                        evidence: Some(evidence.finish()),
                    };
                }
                EffectReceipt::NotApplied { error } => {
                    rollback_remaining(
                        effects,
                        "composite commit stopped after a sub-effect was not applied",
                    )
                    .await;
                    if applied_count == 0 {
                        return EffectReceipt::NotApplied { error };
                    }
                    return EffectReceipt::Applied {
                        durability: EffectDurability::DurabilityFailed(format!(
                            "composite partially applied: {applied_count} earlier sub-effect(s) \
                             landed before a later sub-effect was not applied ({error})"
                        )),
                        evidence: Some(evidence.finish()),
                    };
                }
                EffectReceipt::Unknown { error } => {
                    rollback_remaining(
                        effects,
                        "composite commit stopped after a sub-effect returned an unknown state",
                    )
                    .await;
                    if applied_count == 0 {
                        return EffectReceipt::Unknown { error };
                    }
                    return EffectReceipt::Applied {
                        durability: EffectDurability::DurabilityFailed(format!(
                            "composite partially applied: {applied_count} earlier sub-effect(s) \
                             definitely landed and a later sub-effect has unknown state ({error})"
                        )),
                        evidence: Some(evidence.finish()),
                    };
                }
            }
        }
        EffectReceipt::Applied {
            durability: EffectDurability::Durable,
            evidence: Some(evidence.finish()),
        }
    }

    async fn rollback(self: Box<Self>, reason: &str) {
        for effect in (*self).into_iter() {
            effect.rollback(reason).await;
        }
    }
}

const MAX_COMPOSITE_EVIDENCE_CHARS: usize = 2_000;
const COMPOSITE_EVIDENCE_TRUNCATED: &str = "...[evidence truncated]";

/// A child can supply an arbitrary evidence string. Keep the aggregate
/// receipt bounded even when a composite contains many children or a child
/// returns an unexpectedly large reference.
#[derive(Default)]
struct CompositeEvidence {
    value: String,
    chars: usize,
    truncated: bool,
}

impl CompositeEvidence {
    fn push_optional(&mut self, value: Option<String>) {
        if let Some(value) = value {
            self.push(value);
        }
    }

    fn push(&mut self, value: String) {
        if value.is_empty() || self.truncated {
            return;
        }
        if !self.value.is_empty() {
            if self.chars == MAX_COMPOSITE_EVIDENCE_CHARS {
                self.mark_truncated();
                return;
            }
            self.value.push(',');
            self.chars += 1;
        }
        for ch in value.chars() {
            if self.chars == MAX_COMPOSITE_EVIDENCE_CHARS {
                self.mark_truncated();
                return;
            }
            self.value.push(ch);
            self.chars += 1;
        }
    }

    fn mark_truncated(&mut self) {
        if self.truncated {
            return;
        }
        let marker_chars = COMPOSITE_EVIDENCE_TRUNCATED.chars().count();
        let retained_chars = MAX_COMPOSITE_EVIDENCE_CHARS.saturating_sub(marker_chars);
        while self.chars > retained_chars {
            self.value.pop();
            self.chars -= 1;
        }
        self.value.push_str(COMPOSITE_EVIDENCE_TRUNCATED);
        self.chars += marker_chars;
        self.truncated = true;
    }

    fn finish(self) -> String {
        self.value
    }
}

async fn rollback_remaining(effects: std::vec::IntoIter<Box<dyn Effect>>, reason: &str) {
    for effect in effects {
        effect.rollback(reason).await;
    }
}

/// Why an effect commit failed. The distinction is load-bearing: after a
/// `NotApplied` failure the world is unchanged (tell the model "nothing
/// happened"); after `AppliedButDurabilityFailed` the side effect already
/// landed but its journal record could not be persisted — the runtime must
/// surface a degraded/recovery state instead of claiming the mutation never
/// happened.
#[derive(Debug)]
pub enum EffectCommitError {
    /// The effect did not land; there is nothing to recover.
    NotApplied(AgentError),
    /// The effect landed but its durability record (journal) failed. The
    /// filesystem and the journal now disagree.
    AppliedButDurabilityFailed(AgentError),
}

impl std::fmt::Display for EffectCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApplied(error) => write!(f, "effect not applied: {error}"),
            Self::AppliedButDurabilityFailed(error) => {
                write!(f, "effect applied but its journal record failed: {error}")
            }
        }
    }
}

impl std::error::Error for EffectCommitError {}

/// A structured completion proposal the model attaches to its last tool
/// call (`task.complete`). The runtime validates it and commits it as the
/// active task's typed `CompletionRecord` at the turn's safe point — after
/// the turn commits, never mid-operation — through the same CTX-10
/// transaction the `/done` path uses. Every field is bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionProposal {
    /// Bounded completion summary (becomes the CompletionRecord summary).
    pub summary: String,
    /// Bounded `artifact://` refs the completion produced, recorded on the
    /// CompletionRecord so the evidence stays attached to the outcome.
    #[serde(default)]
    pub artifacts: Vec<String>,
}

/// A directive a tool attaches to its output asking the runtime to change
/// runtime-owned state (a context `gc_hint` / `tag` / `lease` / `collect`,
/// or a structured task completion). Unlike a plain `ToolOutput` field —
/// which any tool, including a capability, could set — a `RuntimeDirective`
/// is a distinct `ToolOutcome` variant. The dispatcher only lets trusted
/// tools and capabilities holding `RUNTIME_CONTEXT_CONTROL` produce it, so
/// an arbitrary capability cannot forge runtime-control requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeDirective {
    Context(ContextAction),
    /// The model proposes completing the active task with a typed outcome.
    /// Executed at the turn's safe point (after the turn commits), so the
    /// completion never races an in-flight operation.
    CompleteTask(CompletionProposal),
}

/// Permission a capability's manifest must declare to attach context
/// directives to its outputs. The dispatcher checks it before a directive
/// from a capability reaches the actor.
pub const RUNTIME_CONTEXT_CONTROL: &str = "runtime:context-control";

/// What a tool execution produced: either a plain bounded output (a value —
/// reads, searches, already-applied behavior like a spawned process), an
/// output plus a staged side effect the runtime must commit (or roll back)
/// after validating the operation is still current, an output plus a
/// runtime directive (context control) the actor executes at commit time,
/// or an output plus an engine query the runtime resolves against the
/// context engine (tools never touch the engine — invariant 3).
pub enum ToolOutcome {
    /// The execution produced only an output; there is nothing to commit.
    Value(ToolOutput),
    /// The computation finished and a side effect is staged. `output` is
    /// what the model sees after the runtime commits the effect.
    PreparedEffect {
        output: ToolOutput,
        effect: Box<dyn Effect>,
    },
    /// The computation finished and the tool asks the runtime to change
    /// runtime-owned state. Executed at operation-commit time, right after
    /// any staged effect, so "manual collect now" is actually now.
    RuntimeDirective {
        output: ToolOutput,
        directive: RuntimeDirective,
    },
    /// The computation finished and the tool asks the runtime to resolve a
    /// read-only query against the context engine (search/inspect/fetch
    /// over externalized refs). The placeholder `output` becomes the final
    /// tool output once the engine answers.
    EngineQuery {
        output: ToolOutput,
        query: EngineQuery,
    },
}

/// A read-only query the runtime resolves against the context engine. Only
/// builtin context tools produce these (a capability cannot emit an engine
/// query through `CapabilityOutcome`), and they carry no side effects — the
/// engine stamps access on fetch so recency ranking stays honest.
#[derive(Debug, Clone)]
pub enum EngineQuery {
    /// Deterministic search over externalized refs (entity/kind/scope/task
    /// /label filters + recency). `limit` caps the answer.
    SearchExternal {
        query: String,
        kind: Option<ContextKind>,
        scope: Option<ContextScope>,
        task_id: Option<TaskId>,
        label: Option<String>,
        limit: usize,
    },
    /// Metadata of one externalized entry by item id (no store read).
    InspectExternal { item_id: ContextItemId },
    /// Full content of one externalized item, pulled back deliberately.
    FetchExternal { item_id: ContextItemId },
}

impl std::fmt::Debug for ToolOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolOutcome::Value(output) => f.debug_tuple("Value").field(output).finish(),
            ToolOutcome::PreparedEffect { output, .. } => f
                .debug_struct("PreparedEffect")
                .field("output", output)
                .field("effect", &"<staged effect>")
                .finish(),
            ToolOutcome::RuntimeDirective { output, directive } => f
                .debug_struct("RuntimeDirective")
                .field("output", output)
                .field("directive", directive)
                .finish(),
            ToolOutcome::EngineQuery { output, query } => f
                .debug_struct("EngineQuery")
                .field("output", output)
                .field("query", query)
                .finish(),
        }
    }
}

/// Maximum number of exact tool requirements one task may retain. This is a
/// runtime resource-policy bound, not a model-context packing hint.
pub const MAX_TASK_TOOL_REQUIREMENTS: usize = 32;
/// Defensive bounds for task-owned requirement metadata and event rows.
pub const MAX_TOOL_REQUIREMENT_NAME_CHARS: usize = 96;
pub const MAX_TOOL_REQUIREMENT_REASON_CHARS: usize = 160;
/// Defensive bounds for the actor-owned TaskAnchor contract.
/// Each free-text anchor field is capped; each list (constraints, criteria,
/// plan steps, open loops) and each typed root-claim list is capped in
/// length and per-entry size. An event row names at most this many changed
/// fields so the audit event stays bounded regardless of anchor size.
pub const MAX_TASK_ANCHOR_TEXT_CHARS: usize = 2_000;
pub const MAX_TASK_ANCHOR_LIST_ITEMS: usize = 32;
pub const MAX_TASK_ANCHOR_ITEM_CHARS: usize = 200;
pub const MAX_TASK_ANCHOR_CLAIMS: usize = 32;
pub const MAX_TASK_ANCHOR_CHANGED_FIELDS: usize = 8;
/// Defensive bounds for one typed CompletionRecord.
pub const MAX_COMPLETION_SUMMARY_CHARS: usize = 2_000;
pub const MAX_COMPLETION_REF_CHARS: usize = 256;
pub const MAX_COMPLETION_ARTIFACTS: usize = 32;
pub const MAX_TOOL_SURFACE_REPORT_SELECTED: usize = 32;
pub const MAX_TOOL_SURFACE_REPORT_OMITTED: usize = 32;
pub const MAX_TOOL_SURFACE_REPORT_BLOCKED: usize = 32;
/// UTF-8 byte cap for every tool name copied into a round-plan event row.
pub const MAX_TOOL_SURFACE_REPORT_NAME_BYTES: usize = 64;
/// Hard wire cap for one serialized round-surface report event. Covers the
/// worst-case fixed overhead of every row at all row/name maxima, including
/// the per-row `origin` provenance field.
pub const MAX_TOOL_SURFACE_REPORT_WIRE_BYTES: usize = 18 * 1024;

/// A task's demand for one exact tool id. This is orthogonal to catalog
/// lifecycle and authority: it neither enables a capability nor grants an
/// approval/effect permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceDemand {
    KeepReady,
    PreferSurface,
    MustSurface,
}

impl ToolSurfaceDemand {
    /// Explicit priority for conflict resolution and deterministic reports.
    pub const fn rank(self) -> u8 {
        match self {
            Self::KeepReady => 1,
            Self::PreferSurface => 2,
            Self::MustSurface => 3,
        }
    }
}

/// One exact, bounded task-owned tool requirement. `reason` is explanatory
/// metadata only; it never enters a provider request or changes authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceRequirement {
    pub tool_name: String,
    pub demand: ToolSurfaceDemand,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceOmissionReason {
    KeepReady,
    SchemaBudget,
    ProviderInputBudget,
    Unavailable,
}

impl ToolSurfaceOmissionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KeepReady => "kept ready outside the prompt",
            Self::SchemaBudget => "round schema budget",
            Self::ProviderInputBudget => "provider input budget",
            Self::Unavailable => "not available at the safe point",
        }
    }
}

/// Why a MustSurface requirement prevented a model request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceBlockReason {
    Unavailable,
    SchemaBudget,
    ProviderInputBudget,
}

/// Which authority put one tool into the round surface. Rows with the same
/// `demand` can now answer whether they entered because of Task intent, a
/// fail-closed dispatcher/core policy, or an ordinary catalog load. `Unknown`
/// marks rows predating per-row provenance (old journal events), so no
/// legacy row ever pretends to know its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceOrigin {
    /// Legacy row without provenance information.
    #[default]
    Unknown,
    /// Task-owned requirement (task requirement revision / reason ref).
    TaskRequirement,
    /// Fail-closed dispatcher/core policy (`may_omit` returned false).
    DispatcherRequired,
    /// Loaded optional from the catalog (e.g. `capability.load`).
    CatalogLoadedOptional,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceSelection {
    pub tool_name: String,
    pub demand: ToolSurfaceDemand,
    /// Which authority put this tool into consideration.
    #[serde(default)]
    pub origin: ToolSurfaceOrigin,
    pub approx_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceOmission {
    pub tool_name: String,
    pub demand: ToolSurfaceDemand,
    /// Which authority put this tool into consideration.
    #[serde(default)]
    pub origin: ToolSurfaceOrigin,
    pub reason: ToolSurfaceOmissionReason,
    pub approx_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceBlock {
    pub tool_name: String,
    pub demand: ToolSurfaceDemand,
    pub reason: ToolSurfaceBlockReason,
}

/// Exact source revisions used to derive one round surface. Optional fields
/// stay `None` until the corresponding policy plane exists; zero never
/// pretends to be a real Task/Focus/policy revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfaceSourceRevisions {
    pub builtin_catalog_generation: u64,
    pub capability_catalog_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_requirement_revision: Option<u64>,
    /// TaskAnchor revision the derived roots came from. None when no task
    /// is active (focus-only rounds have no anchor plane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_policy_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfacePlanStatus {
    Ready,
    Unsatisfiable { reason: ToolSurfaceBlockReason },
}

/// Bounded, schema-free audit record for one round's surface decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSurfacePlanReport {
    pub turn_id: TurnId,
    pub model_round: usize,
    pub surface_revision: u64,
    pub source_revisions: ToolSurfaceSourceRevisions,
    pub status: ToolSurfacePlanStatus,
    pub selected: Vec<ToolSurfaceSelection>,
    pub selected_total: usize,
    pub omitted: Vec<ToolSurfaceOmission>,
    pub omitted_total: usize,
    pub blocked: Vec<ToolSurfaceBlock>,
    pub blocked_total: usize,
    pub selected_schema_tokens: usize,
    pub mandatory_schema_tokens: usize,
    pub estimated_input_tokens: usize,
    pub input_budget_tokens: usize,
}

/// One immutable view of the tool surface captured at a runtime safe point.
/// A model round publishes exactly one final snapshot and uses it for the
/// budget, prompt and tool-call validation. `generation` remains a legacy
/// catalog display value; `surface_revision` is the unique round identity and
/// `source_revisions` preserves the non-colliding source generations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolSurfaceSnapshot {
    pub specs: Vec<ToolSpec>,
    pub generation: u64,
    #[serde(default)]
    pub surface_revision: u64,
    #[serde(default)]
    pub source_revisions: ToolSurfaceSourceRevisions,
    /// Bounded host-only decisions used for precise call rejection. Valid
    /// tool names remain exact up to `MAX_TOOL_REQUIREMENT_NAME_CHARS`; the
    /// tighter event-display cap is applied only when constructing
    /// `ToolSurfacePlanReport`. Full diagnostics never enter the prompt.
    #[serde(default)]
    pub omissions: Vec<ToolSurfaceOmission>,
    #[serde(default)]
    pub omitted_total: usize,
}

/// The unified tool/capability lifecycle shared by the builtin catalog and
/// dynamic capabilities. `Available` entries are known but not on the model
/// surface; `Loaded` entries are offered to the model; `Active` is executing
/// right now; `Warm`/`Unloaded` are the idle-cooling steps of the builtin
/// catalog. Disabled/quarantined states map onto `Unloaded` in the current
/// runtime — the maturity ladder (`CapabilityStatus`) carries that policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolLifecycle {
    /// Known to the catalog but not offered to the model.
    Available,
    /// In the active set: its schema is exposed to the model.
    Loaded,
    /// Executing a call right now.
    Active,
    /// Idle; kept only for a fast reload.
    Warm,
    /// Removed from the model surface by the GC or an explicit unload.
    Unloaded,
}

impl ToolLifecycle {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Loaded => "loaded",
            Self::Active => "active",
            Self::Warm => "warm",
            Self::Unloaded => "unloaded",
        }
    }

    /// Whether the tool's schema is part of the model surface right now.
    pub fn in_surface(self) -> bool {
        matches!(self, Self::Loaded | Self::Active)
    }
}

/// One row of the unified tool catalog (the discovery surface shared by
/// `capability.search` / `capability.inspect`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCatalogEntry {
    pub name: String,
    pub state: ToolLifecycle,
    /// Who owns the tool: `builtin` or the capability id (e.g. `ext:github`).
    pub owner: String,
    pub description: String,
    /// Declared risk class; searchable via the provider-owned catalog index.
    #[serde(default)]
    pub risk: ToolRisk,
}

/// Always-visible control tools of the unified catalog: the model can
/// discover and change the active set no matter what else is loaded.
pub const CAPABILITY_SEARCH: &str = "capability.search";
pub const CAPABILITY_LOAD: &str = "capability.load";
pub const CAPABILITY_UNLOAD: &str = "capability.unload";
pub const CAPABILITY_INSPECT: &str = "capability.inspect";

/// The merged control surface: one `capability.manage` entry point (op =
/// search/inspect/load/unload) and one `context.manage` entry point (op =
/// tag/lease/collect/search/inspect/fetch/admit/derive) keep the
/// always-visible schema count small. `gc_hint` is not model-facing:
/// collection stays engine-owned.
pub const CAPABILITY_MANAGE: &str = "capability.manage";
pub const CONTEXT_MANAGE: &str = "context.manage";

/// `capability.search` paging bounds. A large catalog (hundreds of
/// capabilities) must never become context pollution: the model-facing
/// page is capped, and the full listing spills to an artifact.
pub const CAPABILITY_SEARCH_DEFAULT_LIMIT: usize = 20;
pub const CAPABILITY_SEARCH_MAX_LIMIT: usize = 50;

#[async_trait]
pub trait ToolDispatcher: Send + Sync {
    /// The current tool surface. MUST be pure: a model round reads it for
    /// the budget, the prompt and tool-call validation, so the surface has
    /// to stay stable within a round.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Capture the tool surface for one model round. The runtime calls this
    /// once per round, right after `gc()`, and threads the snapshot through
    /// budget, prompt and validation so the model always sees — and the
    /// runtime always validates against — the same surface.
    fn snapshot(&self) -> ToolSurfaceSnapshot {
        ToolSurfaceSnapshot {
            specs: self.specs(),
            generation: 0,
            source_revisions: ToolSurfaceSourceRevisions::default(),
            ..ToolSurfaceSnapshot::default()
        }
    }

    /// Whether this tool's schema may be omitted from one model round when
    /// the provider's final input budget is smaller than the captured tool
    /// surface. This is a pure classification query: omitting a schema from
    /// a round must not unload the tool or otherwise change catalog state.
    ///
    /// The default is fail-closed because a dispatcher that cannot
    /// distinguish its core/required tools from optional tools must never
    /// let token pressure silently hide one of them.
    fn may_omit_from_round(&self, _name: &str) -> bool {
        false
    }

    /// Explicit lifecycle maintenance safe point, called by the runtime
    /// once per model round. Dispatchers with mutable tool lifecycles
    /// (idle aging, unloading) run their GC here — never inside `specs()`.
    ///
    /// `roots` names the tools the runtime must not age out: the active
    /// task's tool-demand set (TaskAnchor-driven tool roots). A root tool
    /// may still be unloaded explicitly; roots only protect against the
    /// silent idle path. The default ignores roots.
    fn gc(&self, _roots: &[String]) {}

    /// Unified discovery surface: every known tool (builtin and dynamic
    /// capability) with its lifecycle state and owner, for
    /// `capability.search` / `capability.inspect`. Default: no rows, so
    /// dispatchers without a catalog keep working unchanged.
    fn catalog(&self) -> Vec<ToolCatalogEntry> {
        Vec::new()
    }

    /// Unified `capability.load`: put a tool (or the capability owning it)
    /// on the model surface. Default: unsupported.
    fn load_tool(&self, name: &str) -> AgentResult<()> {
        Err(AgentError::InvalidRequest(format!(
            "this tool provider does not support loading '{name}'"
        )))
    }

    /// Unified `capability.unload`: remove a tool from the model surface.
    fn unload_tool(&self, name: &str) -> AgentResult<()> {
        Err(AgentError::InvalidRequest(format!(
            "this tool provider does not support unloading '{name}'"
        )))
    }

    /// Unified `capability.inspect`: the full spec of one tool, if known.
    fn inspect_tool(&self, _name: &str) -> Option<ToolSpec> {
        None
    }

    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn effect_intent_round_trips_and_maps_to_risk() {
        for intent in [
            EffectIntent::ReadOnly,
            EffectIntent::WorkspaceWrite {
                path: "src/main.rs".into(),
                content_bytes: 42,
            },
            EffectIntent::ProcessRun {
                command: "cargo test".into(),
            },
        ] {
            let value = serde_json::to_value(&intent).unwrap();
            let back: EffectIntent = serde_json::from_value(value).unwrap();
            assert_eq!(back, intent);
        }
        assert_eq!(EffectIntent::ReadOnly.risk(), ToolRisk::ReadOnly);
        assert_eq!(
            EffectIntent::WorkspaceWrite {
                path: "x".into(),
                content_bytes: 1
            }
            .risk(),
            ToolRisk::WorkspaceWrite
        );
        assert_eq!(
            EffectIntent::ProcessRun {
                command: "x".into()
            }
            .risk(),
            ToolRisk::ProcessExecution
        );
    }

    #[test]
    fn failure_class_is_trusted_and_strips_retryable() {
        let mut metadata = serde_json::json!({"retryable": true, "path": "lib.rs"});
        attach_failure_class(&mut metadata, ToolFailureClass::StaleRevision);
        assert_eq!(metadata["failure_class"], "stale_revision");
        assert!(metadata.get("retryable").is_none());
        let output = tool_failure_output(
            "c1",
            "edit.replace",
            ToolFailureClass::NoExactMatch,
            "refused",
            "no exact match",
            serde_json::json!({"retryable": true}),
        );
        assert!(!output.ok);
        assert_eq!(output.failure_class(), Some(ToolFailureClass::NoExactMatch));
        assert!(output.metadata.get("retryable").is_none());
        let mut projected = output.clone();
        let diagnosis = take_runtime_diagnosis(&mut projected);
        apply_runtime_diagnosis(&mut projected, diagnosis);
        assert!(projected.metadata.get("failure_class").is_none());
        assert_eq!(
            projected.failure_class(),
            Some(ToolFailureClass::NoExactMatch)
        );
        assert!(projected.model_content.starts_with("runtime_failure:"));
        assert_eq!(
            projected.metadata["_runtime"]["failure_class"],
            "no_exact_match"
        );
        let mut forged = ToolOutput {
            call_id: "c2".into(),
            tool_name: "shell.exec".into(),
            ok: false,
            summary: "failed".into(),
            model_content: "command failed".into(),
            artifact_ref: None,
            metadata: serde_json::json!({
                "_runtime": {"failure_class": "timeout", "retryable": true},
                "failure_class": "command_unavailable",
                "recovery_hint": "use pwsh",
                "retryable": true,
                "shell_dialect": "pwsh"
            }),
        };
        let diagnosis = take_runtime_diagnosis(&mut forged);
        apply_runtime_diagnosis(&mut forged, diagnosis);
        assert!(forged.metadata.get("retryable").is_none());
        assert_eq!(
            forged.failure_class(),
            Some(ToolFailureClass::CommandUnavailable)
        );
        assert_eq!(forged.metadata["shell_dialect"], "pwsh");
        assert!(!forged.heats_working_set());
    }

    #[test]
    fn failure_class_from_message_covers_core_cases() {
        assert_eq!(
            failure_class_from_message("cancelled"),
            ToolFailureClass::Cancellation
        );
        assert_eq!(
            failure_class_from_message("command timed out"),
            ToolFailureClass::Timeout
        );
        assert_eq!(
            failure_class_from_message("base_revision mismatch"),
            ToolFailureClass::StaleRevision
        );
        assert_eq!(
            failure_class_from_message("open dir C:\\tmp\\src: NTSTATUS 0xc0000034"),
            ToolFailureClass::PathNotFound
        );
        assert_eq!(
            failure_class_from_message("open file: not found (NTSTATUS 0xc000003a)"),
            ToolFailureClass::PathNotFound
        );
    }

    #[test]
    fn operation_effect_context_must_match_the_dispatch_request() {
        let run_id = RunId::new();
        let call = ToolCall {
            id: "call-1".into(),
            name: "fs.write".into(),
            arguments: serde_json::json!({"path": "src/lib.rs", "content": "x"}),
        };
        let context = OperationEffectContext {
            identity: ToolOperationIdentity {
                run_id,
                task_id: None,
                turn_id: TurnId::new(),
                scope_id: None,
                operation_id: crate::OperationId::new(),
                generation: 1,
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                argument_digest: crate::ArgumentDigest::from_json(&call.arguments),
            },
            effect_id: EffectId::new(),
        };
        let request = ToolExecutionRequest {
            run_id,
            call: call.clone(),
            effect_context: Some(context.clone()),
            cancel: CancellationToken::new(),
        };
        assert!(request.validate().is_ok());

        let mut mismatched = request.clone();
        mismatched.call.arguments = serde_json::json!({"path": "src/lib.rs", "content": "y"});
        assert_eq!(
            mismatched.validate().unwrap_err(),
            "operation effect context does not match the tool request"
        );

        let mut nil_effect = context;
        nil_effect.effect_id = EffectId(uuid::Uuid::nil());
        assert_eq!(
            nil_effect.validate().unwrap_err(),
            "operation effect context contains a nil effect id"
        );
    }

    #[test]
    fn legacy_serialized_tool_request_defaults_to_no_effect_context() {
        let request: ToolExecutionRequest = serde_json::from_value(serde_json::json!({
            "run_id": RunId::new(),
            "call": {"id": "call-1", "name": "fs.read", "arguments": {}}
        }))
        .unwrap();
        assert!(request.effect_context.is_none());
        assert!(request.validate().is_ok());
    }

    /// Records whether it was committed or rolled back, optionally failing
    /// its own commit — the observable trace for the composite semantics.
    #[derive(Clone, Copy)]
    enum RecordingCommit {
        Durable,
        NotApplied,
        DurabilityFailed,
        Unknown,
    }

    struct RecordingEffect {
        label: String,
        commits: Arc<AtomicUsize>,
        rollbacks: Arc<AtomicUsize>,
        result: RecordingCommit,
    }

    #[async_trait::async_trait]
    impl Effect for RecordingEffect {
        fn describe(&self) -> String {
            self.label.clone()
        }
        async fn commit(self: Box<Self>) -> EffectReceipt {
            self.commits.fetch_add(1, Ordering::SeqCst);
            match self.result {
                RecordingCommit::Durable => EffectReceipt::Applied {
                    durability: EffectDurability::Durable,
                    evidence: Some(self.label),
                },
                RecordingCommit::NotApplied => EffectReceipt::NotApplied {
                    error: "boom".into(),
                },
                RecordingCommit::DurabilityFailed => EffectReceipt::Applied {
                    durability: EffectDurability::DurabilityFailed("journal unavailable".into()),
                    evidence: Some(self.label),
                },
                RecordingCommit::Unknown => EffectReceipt::Unknown {
                    error: "timeout".into(),
                },
            }
        }
        async fn rollback(self: Box<Self>, _reason: &str) {
            self.rollbacks.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn composite(effects: Vec<Box<dyn Effect>>) -> Box<dyn Effect> {
        Box::new(effects)
    }

    #[tokio::test]
    async fn composite_effect_commits_every_sub_effect_in_order() {
        let commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
            Box::new(RecordingEffect {
                label: "b".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
            Box::new(RecordingEffect {
                label: "c".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
        ]);
        let receipt = effect.commit().await;
        assert!(
            matches!(
                &receipt,
                EffectReceipt::Applied {
                    durability: EffectDurability::Durable,
                    evidence,
                } if evidence.as_deref() == Some("a,b,c")
            ),
            "all three sub-effects commit and their evidence is aggregated: {receipt:?}"
        );
        assert_eq!(commits.load(Ordering::SeqCst), 3);
        assert_eq!(rollbacks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn composite_effect_reports_partial_application_and_cleans_the_remainder() {
        // Once `a` lands, `b` cannot make the aggregate claim that nothing
        // happened. `c` must not commit and its preparation must be cleaned.
        let ca = Arc::new(AtomicUsize::new(0));
        let cb = Arc::new(AtomicUsize::new(0));
        let cc = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a".into(),
                commits: ca.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
            Box::new(RecordingEffect {
                label: "b".into(),
                commits: cb.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::NotApplied,
            }),
            Box::new(RecordingEffect {
                label: "c".into(),
                commits: cc.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
        ]);
        let receipt = effect.commit().await;
        match receipt {
            EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(error),
                evidence,
            } => {
                assert!(error.contains("composite partially applied"));
                assert_eq!(evidence.as_deref(), Some("a"));
            }
            other => panic!("the composite must report the application truth: {other:?}"),
        }
        assert_eq!(ca.load(Ordering::SeqCst), 1, "'a' committed first");
        assert_eq!(cb.load(Ordering::SeqCst), 1, "'b' attempted and failed");
        assert_eq!(
            cc.load(Ordering::SeqCst),
            0,
            "'c' must never run after the failure"
        );
        assert_eq!(
            rollbacks.load(Ordering::SeqCst),
            1,
            "the unattempted 'c' preparation must be cleaned"
        );
    }

    #[tokio::test]
    async fn composite_effect_preserves_not_applied_when_nothing_landed() {
        let first_commits = Arc::new(AtomicUsize::new(0));
        let later_commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a".into(),
                commits: first_commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::NotApplied,
            }),
            Box::new(RecordingEffect {
                label: "b".into(),
                commits: later_commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
        ]);

        assert!(matches!(
            effect.commit().await,
            EffectReceipt::NotApplied { error } if error == "boom"
        ));
        assert_eq!(first_commits.load(Ordering::SeqCst), 1);
        assert_eq!(later_commits.load(Ordering::SeqCst), 0);
        assert_eq!(rollbacks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn composite_effect_preserves_unknown_when_nothing_definitely_landed() {
        let commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Unknown,
            }),
            Box::new(RecordingEffect {
                label: "b".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
        ]);

        assert!(matches!(
            effect.commit().await,
            EffectReceipt::Unknown { error } if error == "timeout"
        ));
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(rollbacks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn composite_effect_reports_definite_partial_application_before_unknown() {
        let commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
            Box::new(RecordingEffect {
                label: "b".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Unknown,
            }),
            Box::new(RecordingEffect {
                label: "c".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
        ]);

        assert!(matches!(
            effect.commit().await,
            EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(error),
                evidence: Some(evidence),
            } if error.contains("definitely landed") && evidence == "a"
        ));
        assert_eq!(commits.load(Ordering::SeqCst), 2);
        assert_eq!(rollbacks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn composite_effect_aggregates_evidence_for_a_durability_failure() {
        let commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
            Box::new(RecordingEffect {
                label: "b".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::DurabilityFailed,
            }),
            Box::new(RecordingEffect {
                label: "c".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
        ]);

        assert!(matches!(
            effect.commit().await,
            EffectReceipt::Applied {
                durability: EffectDurability::DurabilityFailed(error),
                evidence: Some(evidence),
            } if error == "journal unavailable" && evidence == "a,b"
        ));
        assert_eq!(commits.load(Ordering::SeqCst), 2);
        assert_eq!(rollbacks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn composite_effect_bounds_aggregated_evidence() {
        let commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![Box::new(RecordingEffect {
            label: "界".repeat(MAX_COMPOSITE_EVIDENCE_CHARS + 10),
            commits,
            rollbacks,
            result: RecordingCommit::Durable,
        })]);

        let EffectReceipt::Applied {
            evidence: Some(evidence),
            ..
        } = effect.commit().await
        else {
            panic!("a durable child must produce an applied aggregate");
        };
        assert_eq!(evidence.chars().count(), MAX_COMPOSITE_EVIDENCE_CHARS);
        assert!(evidence.ends_with(COMPOSITE_EVIDENCE_TRUNCATED));
    }

    #[tokio::test]
    async fn composite_effect_rolls_back_every_sub_effect() {
        let commits = Arc::new(AtomicUsize::new(0));
        let rollbacks = Arc::new(AtomicUsize::new(0));
        let effect = composite(vec![
            Box::new(RecordingEffect {
                label: "a".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
            Box::new(RecordingEffect {
                label: "b".into(),
                commits: commits.clone(),
                rollbacks: rollbacks.clone(),
                result: RecordingCommit::Durable,
            }),
        ]);
        effect.rollback("superseded").await;
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(rollbacks.load(Ordering::SeqCst), 2);
    }
}
