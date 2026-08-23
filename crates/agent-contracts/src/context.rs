use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use std::collections::HashSet;

use crate::{
    AgentError, AgentResult, ContextItemId, Label, OperationId, ScopeId, TaskId, ToolOutput, TurnId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContextKind {
    Goal,
    Constraint,
    Decision,
    UserMessage,
    AssistantMessage,
    ToolObservation,
    FileObservation,
    Error,
    Summary,
    Note,
}

impl ContextKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Constraint => "constraint",
            Self::Decision => "decision",
            Self::UserMessage => "user_message",
            Self::AssistantMessage => "assistant_message",
            Self::ToolObservation => "tool_observation",
            Self::FileObservation => "file_observation",
            Self::Error => "error",
            Self::Summary => "summary",
            Self::Note => "note",
        }
    }
}

/// Cap on structured resource touches in one `WorkingSetSignal`.
pub const MAX_RESOURCE_TOUCHES: usize = 8;
/// Cap on a single resource path (UTF-8 chars) after slash-normalization.
pub const MAX_RESOURCE_PATH_CHARS: usize = 256;

/// A trusted, path-shaped resource the runtime observed. Heating and
/// residency hints consume this, never raw tool stdout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTouch {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Knowledge-plane answer to "which known resource facts did this
/// operation invalidate?" Orthogonal to [`crate::ToolOutput::may_mutate_workspace`]:
/// a process may be allowed to write the workspace (authority) without
/// proving that every previously observed file identity is dead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationFootprint {
    /// No workspace write is possible.
    None,
    /// Writes are confined to these stamped touches.
    Known(Vec<ResourceTouch>),
    /// A write is possible but the touched set is unknown (`shell.exec`
    /// without `ResourceTouch`, `__pycache__`, and so on).
    Unknown,
}

/// Freshness of one operational resource fact. Unknown workspace mutation
/// marks known identities `NeedsRevalidation`; it must not drop them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceFreshness {
    #[default]
    Fresh,
    NeedsRevalidation,
    Missing,
}

/// Narrow trusted hash/identity lookup. Workspace implements this;
/// tools never see the context engine. The runtime revalidates pending
/// facts at the BeforeModel safe point (hash only, no file body in the
/// prompt, no extra model round).
#[async_trait]
pub trait ResourceVersionOracle: Send + Sync {
    /// SHA-256 hex of the current workspace bytes, or `None` if the path
    /// is absent. Other I/O errors propagate so the caller can skip.
    async fn revision(&self, key: &str) -> crate::AgentResult<Option<String>>;
}

/// Slash-normalize a workspace path for identity (`\` → `/`, trim, strip
/// a leading `./`, then cap length). Empty after trim is empty.
pub fn normalize_resource_path(path: &str) -> String {
    let trimmed = path.trim().replace('\\', "/");
    let stripped = trimmed
        .strip_prefix("./")
        .unwrap_or(trimmed.as_str())
        .trim_start_matches('/');
    stripped.chars().take(MAX_RESOURCE_PATH_CHARS).collect()
}

/// Exact path or basename mention in a current directive. The needle must
/// sit on a path-token boundary, so `util.py` does not match `utils.py`
/// and `a.rs` does not match `ba.rs`. Not semantic similarity.
pub fn path_exactly_in_directive(directive: &str, path: &str) -> bool {
    let path = normalize_resource_path(path);
    if path.is_empty() {
        return false;
    }
    let directive = directive.replace('\\', "/");
    if contains_as_path_token(&directive, &path) {
        return true;
    }
    std::path::Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && *name != path)
        .is_some_and(|name| contains_as_path_token(&directive, name))
}

fn contains_as_path_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        let abs = start + rel;
        let before_ok = abs == 0 || is_path_token_boundary(haystack.as_bytes()[abs - 1]);
        let after = abs + needle.len();
        let after_ok =
            after == haystack.len() || is_path_token_boundary(haystack.as_bytes()[after]);
        if before_ok && after_ok {
            return true;
        }
        start = abs.saturating_add(1);
        if start >= haystack.len() {
            break;
        }
    }
    false
}

fn is_path_token_boundary(byte: u8) -> bool {
    matches!(
        byte,
        b'/' | b'\\'
            | b' '
            | b'\t'
            | b'\n'
            | b'\r'
            | b'"'
            | b'\''
            | b'`'
            | b':'
            | b','
            | b';'
            | b'('
            | b')'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'<'
            | b'>'
    )
}

/// How this path appeared in the last model prompt, independent of
/// engine residency. Selecting a context item is not the same as packing
/// its file body: a Checked identity may be selected as `path@rev`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileExposure {
    SelectedBody,
    SelectedDescriptor,
    ExternalDescriptor,
    NotSelected,
}

impl FileExposure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SelectedBody => "selected-body",
            Self::SelectedDescriptor => "selected-descriptor",
            Self::ExternalDescriptor => "external-descriptor",
            Self::NotSelected => "not-selected",
        }
    }
}

/// Why this `fs.read` happened, given the engine's current catalog.
/// Mutually exclusive; classified against current residency *and* last
/// prompt exposure, not "item was selected".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsRereadClass {
    /// File body was packed into the last prompt.
    PreviouslySelected,
    /// Item was selected but packed as `path@rev`, not the body.
    SelectedDescriptor,
    /// Path appeared only as an EXTERNAL CONTEXT `path@rev` ref.
    ExternalDescriptor,
    ResidentUnselected,
    Warm,
    Stored,
    FirstRead,
}

impl FsRereadClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreviouslySelected => "previously-selected",
            Self::SelectedDescriptor => "selected-descriptor",
            Self::ExternalDescriptor => "external-descriptor",
            Self::ResidentUnselected => "resident-unselected",
            Self::Warm => "warm",
            Self::Stored => "stored",
            Self::FirstRead => "first-read",
        }
    }

    pub fn exposure(self) -> FileExposure {
        match self {
            Self::PreviouslySelected => FileExposure::SelectedBody,
            Self::SelectedDescriptor => FileExposure::SelectedDescriptor,
            Self::ExternalDescriptor => FileExposure::ExternalDescriptor,
            Self::ResidentUnselected | Self::Warm | Self::Stored | Self::FirstRead => {
                FileExposure::NotSelected
            }
        }
    }
}

/// Why the model issued this `fs.read`, combining engine residency with
/// Runtime resource-fact freshness. Mutually exclusive. Engine-only
/// [`FsRereadClass`] remains the GC/residency axis; this is the E2E
/// "why did the model need to re-read?" axis.
///
/// | Motive | Meaning |
/// | `first` | Normal first exploration |
/// | `body-visible-current` | Body was in the last prompt; model trajectory |
/// | `descriptor-only` | Last prompt had identity only; model needed the body |
/// | `protocol-checkpoint-body-missing` | 之前读过、摘要未变，帧里丢了正文 |
/// | `checked-fresh` | Identity known and the body had no clear need |
/// | `needs-revalidation` | Runtime should hash; the model should not `fs.read` |
/// | `warm` | GC moved the body to the eviction buffer |
/// | `stored` | Deeper GC rehydration from the store |
/// | `changed` | Digest actually moved; a reread is justified |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FsReadMotive {
    /// First exploration of this path.
    First,
    /// File body was actually in the last prompt; the model still re-read.
    BodyVisibleCurrent,
    /// Last prompt only had `path@rev` (selected or external descriptor).
    DescriptorOnly,
    /// 正文此前已被模型消费（来源为真实读取、摘要未变），当前帧却只剩身份：
    /// 轮次/检查点边界丢掉了正文。协议层正文缓存服务的正是这一类。
    ProtocolCheckpointBodyMissing,
    /// Runtime already knew `path@revision` was Fresh.
    CheckedFresh,
    /// Runtime was uncertain; VersionOracle should have settled this.
    NeedsRevalidation,
    /// Body was in the warm eviction buffer (GC-induced rehydration).
    Warm,
    /// Body was in the store (GC-induced rehydration).
    Stored,
    /// File bytes actually changed; a reread is justified.
    Changed,
}

impl FsReadMotive {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::First => "first",
            Self::BodyVisibleCurrent => "body-visible-current",
            Self::DescriptorOnly => "descriptor-only",
            Self::ProtocolCheckpointBodyMissing => "protocol-checkpoint-body-missing",
            Self::CheckedFresh => "checked-fresh",
            Self::NeedsRevalidation => "needs-revalidation",
            Self::Warm => "warm",
            Self::Stored => "stored",
            Self::Changed => "changed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "first" => Some(Self::First),
            "body-visible-current" | "selected-current" => Some(Self::BodyVisibleCurrent),
            "descriptor-only" => Some(Self::DescriptorOnly),
            "protocol-checkpoint-body-missing" => Some(Self::ProtocolCheckpointBodyMissing),
            "checked-fresh" => Some(Self::CheckedFresh),
            "needs-revalidation" => Some(Self::NeedsRevalidation),
            "warm" => Some(Self::Warm),
            "stored" => Some(Self::Stored),
            "changed" => Some(Self::Changed),
            _ => None,
        }
    }

    pub fn gc_induced(self) -> bool {
        matches!(self, Self::Warm | Self::Stored)
    }
}

/// Runtime-stamped `ToolOutput.metadata` key for [`FsReadMotive`].
pub const FS_READ_MOTIVE_KEY: &str = "fs_read_motive";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContextScope {
    Message,
    Turn,
    Task,
    Session,
    Pinned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextRetention {
    Ephemeral,
    Working,
    Durable,
    Pinned,
}

/// The attention dimension of an item: how present it is in the current
/// working set. Owned by the per-event residency machine (`maintain`), not
/// by GC. Semantic death lives in `SemanticState`, physical placement in
/// `ContextResidency` — attention alone never means an item is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AttentionState {
    Active,
    Cooling,
    Archived,
}

/// The semantic dimension of an item: is it still *true* in the run's
/// decision/error history, or has a newer item replaced it? `Live` items can
/// be recalled when attention or GC brings them back; every non-Live state
/// is terminal — the materializer excludes them and Context GC never
/// resurrects them. `Tombstoned` replaces the old `Dropped` overload (which
/// mixed ephemeral attention loss with semantic death).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SemanticState {
    #[default]
    Live,
    /// A newer decision superseded this one.
    Superseded {
        /// The decision that replaced it; `None` only for items migrated
        /// from pre-semantic checkpoints.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<ContextItemId>,
    },
    /// A successful tool result verified the error fixed.
    VerifiedFixed {
        /// The observation that verified it; `None` for migrated items.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<ContextItemId>,
    },
    /// The item's information lifecycle ended (TTL/staleness). Permanently
    /// dead from the runtime's perspective; only Storage GC may delete it.
    Tombstoned,
}

/// 分级检索访问信号（`CTX-GC-11`）。
///
/// search 是最弱的在线相关性证据；inspect/fetch 是更强的故意读取；
/// `admit` 是显式驻留动作；consumption ack 是最强的在线信号。弱信号
/// 不得覆盖强信号，search 循环也不得把 Cold 条目钉死。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessSignal {
    /// 从未打过戳，或来自分级落地前的 checkpoint。
    #[default]
    None,
    /// `context.search` 命中。最弱：更强信号出现前最多一次 Cold 老化延迟，
    /// 且受单条目冷却和相同查询预算约束。
    SearchHit,
    /// `inspect_external` 读了描述符（无 body IO）。
    Inspect,
    /// `fetch_external` 读了 store 中的 body。
    Fetch,
    /// `context.admit` 把条目重新拉进 working set。
    Admit,
    /// 模型在打包后的帧里真正消费了该条目（`ContextConsumptionAck`）。
    ConsumptionAck,
}

impl AccessSignal {
    /// 显式、可解释的等级。fetch 观察到 body，因此强于只读描述符的 inspect。
    pub fn rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::SearchHit => 1,
            Self::Inspect => 2,
            Self::Fetch => 3,
            Self::Admit => 4,
            Self::ConsumptionAck => 5,
        }
    }
}

impl SemanticState {
    /// Live items can be recalled by attention or GC.
    pub fn is_live(self) -> bool {
        matches!(self, SemanticState::Live)
    }

    /// Superseded, verified-fixed and tombstoned items are semantically dead:
    /// excluded from the model and never resurrected by Context GC.
    pub fn is_dead(self) -> bool {
        !self.is_live()
    }
}

/// Where an item physically lives. `Resident` means the item sits in the
/// model-visible heap; `Warm` means it was moved to the bounded, reversible
/// eviction buffer by GC and can be reactivated when it becomes relevant
/// again; `Cold` means the buffer overflowed and the item's content now
/// lives in the external context store (a lightweight entry with a
/// `ContextRef` stays visible); `External` means the entry has aged out of
/// the working set entirely and only the store retains it. This is the GC
/// dimension, orthogonal to `AttentionState` and `SemanticState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ContextResidency {
    #[default]
    Resident,
    Warm,
    Cold,
    External,
}

/// Named generational stage of an item, derived from how many full GC
/// passes it survived without being a root. Nursery items are young, Stable
/// items have outlived the generational ladder. The underlying pass count
/// stays observable as `ContextItem::gc_generation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Generation {
    Nursery,
    Working,
    Stable,
}

impl Generation {
    pub fn of(passes: u32) -> Self {
        match passes {
            0..=1 => Generation::Nursery,
            2 => Generation::Working,
            _ => Generation::Stable,
        }
    }
}

/// The semantics of one explicit dependency edge. The dependency graph is
/// typed so GC reachability, supersession and future policies can
/// distinguish *why* an item is referenced instead of treating every edge
/// alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DependencyKind {
    /// The item was created with an entity overlap with the target: the
    /// default link edge recorded at ingest (new item -> prior item). This
    /// is weak affinity, not a citation — it ranks and links, but it is
    /// never a permanent-delete guard.
    #[default]
    #[serde(rename = "shares_entities")]
    SharesEntities,
    /// The item is a fact the model derived from the target ref
    /// (`context.derive`): a new item with its own id, explicitly linked
    /// back to the ref it came from, so traceability survives storage GC.
    #[serde(rename = "derived_from")]
    DerivedFrom,
    /// The item is evidence for the target (a test result, a log, a
    /// reproduction that supports a claim about the target).
    #[serde(rename = "evidence_for")]
    EvidenceFor,
    /// The item verified the target (a later success that verified an
    /// earlier error, a check that confirmed a fix).
    #[serde(rename = "verified_by")]
    VerifiedBy,
    /// The target is an artifact this item produced or consumed (a file,
    /// a trace, a report the item references).
    #[serde(rename = "artifact_of")]
    ArtifactOf,
    /// The item continues the target's line of work (an open loop, a
    /// follow-up that extends the target's outcome).
    #[serde(rename = "continuation")]
    Continuation,
}

impl DependencyKind {
    /// Ranking / affinity graph: the target may influence selection score
    /// or working-set clustering. This is not a prompt-inclusion or
    /// residency root. Auto-minted entity overlap (`SharesEntities`) lives
    /// here, along with evidence and continuation affinity.
    pub fn affects_ranking(self) -> bool {
        matches!(
            self,
            DependencyKind::SharesEntities
                | DependencyKind::EvidenceFor
                | DependencyKind::Continuation
        )
    }

    /// Prompt expansion may copy the target's *body* into the materialized
    /// working set. Affinity and provenance are not citations-in-the-prompt:
    /// only a continuation of the same line of work pulls the prior body.
    pub fn requires_prompt_body(self) -> bool {
        matches!(self, DependencyKind::Continuation)
    }

    /// Full-GC mark/reactivate may treat the target as required evidence of
    /// a live root (Warm → Resident, bypassing closed-scope guards).
    /// Provenance (`DerivedFrom`) and weak affinity must not resurrect
    /// compacted or merely-related items.
    pub fn requires_residency(self) -> bool {
        matches!(self, DependencyKind::Continuation)
    }

    /// Storage GC reachability: a deliberate citation that must keep the
    /// target's blob from permanent deletion. Weak affinity never pins
    /// storage. Alias of [`Self::is_strong`].
    pub fn protects_storage(self) -> bool {
        !matches!(self, DependencyKind::SharesEntities)
    }

    /// Whether this edge is a *strong* citation: a deliberate,
    /// content-preserving reference that must survive permanent deletion.
    /// Weak affinity (`SharesEntities`) is auto-minted from entity
    /// overlap at ingest and must not pin terminal records forever.
    ///
    /// This is the storage-GC predicate. Prompt expansion and residency
    /// mark use [`Self::requires_prompt_body`] and
    /// [`Self::requires_residency`] instead — a strong citation is not a
    /// prompt or heap root.
    pub fn is_strong(self) -> bool {
        self.protects_storage()
    }
}

/// One explicit dependency edge: the item depends on `target` for the
/// given reason. Deserializes from both the typed form
/// (`{"target": "...", "kind": "shares_entities"}`) and the pre-typed
/// form (`"<uuid>"`, meaning `SharesEntities`), so checkpoints written
/// before the graph was typed keep loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DependencyEdge {
    pub target: ContextItemId,
    #[serde(default)]
    pub kind: DependencyKind,
}

impl DependencyEdge {
    pub fn shares(target: ContextItemId) -> Self {
        Self {
            target,
            kind: DependencyKind::SharesEntities,
        }
    }

    pub fn derived_from(target: ContextItemId) -> Self {
        Self {
            target,
            kind: DependencyKind::DerivedFrom,
        }
    }

    pub fn evidence_for(target: ContextItemId) -> Self {
        Self {
            target,
            kind: DependencyKind::EvidenceFor,
        }
    }

    pub fn verified_by(target: ContextItemId) -> Self {
        Self {
            target,
            kind: DependencyKind::VerifiedBy,
        }
    }

    pub fn artifact_of(target: ContextItemId) -> Self {
        Self {
            target,
            kind: DependencyKind::ArtifactOf,
        }
    }

    pub fn continuation(target: ContextItemId) -> Self {
        Self {
            target,
            kind: DependencyKind::Continuation,
        }
    }

    pub fn affects_ranking(self) -> bool {
        self.kind.affects_ranking()
    }

    pub fn requires_prompt_body(self) -> bool {
        self.kind.requires_prompt_body()
    }

    pub fn requires_residency(self) -> bool {
        self.kind.requires_residency()
    }

    pub fn protects_storage(self) -> bool {
        self.kind.protects_storage()
    }
}

impl<'de> Deserialize<'de> for DependencyEdge {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Typed {
                target: ContextItemId,
                #[serde(default)]
                kind: DependencyKind,
            },
            Legacy(ContextItemId),
        }
        match Repr::deserialize(deserializer)? {
            Repr::Typed { target, kind } => Ok(DependencyEdge { target, kind }),
            Repr::Legacy(target) => Ok(DependencyEdge::shares(target)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: ContextItemId,
    pub task_id: Option<TaskId>,
    /// The scope this item was produced in, from the runtime-driven scope
    /// tree. Authoritative membership: a closed scope promotes/evicts the
    /// items carrying its id. `None` only for items created before scope
    /// tracking existed (e.g. restored old checkpoints).
    #[serde(default)]
    pub scope_id: Option<ScopeId>,
    pub content: String,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub retention: ContextRetention,
    /// Attention state (Active/Cooling/Archived), owned by the residency
    /// machine. `alias = "state"` keeps pre-split checkpoints loadable.
    #[serde(alias = "state")]
    pub attention: AttentionState,
    /// Semantic state (Live/Superseded/VerifiedFixed/Tombstoned): terminal
    /// death is expressed here, never in attention.
    #[serde(default)]
    pub semantic: SemanticState,
    pub importance: f32,
    pub relevance: f32,
    pub created_tick: u64,
    pub last_access_tick: u64,
    pub access_count: u32,
    #[serde(default)]
    pub created_turn: u64,
    #[serde(default)]
    pub last_access_turn: u64,
    /// Last user turn this item was selected into a materialized model
    /// surface, stamped on consumption acknowledgement. Previews are
    /// non-consuming and never stamp it, so merely materializing never
    /// ages an item's recency: recency scoring reads this clock, never the
    /// event sequence.
    #[serde(default)]
    pub last_selected_turn: u64,
    /// Explicit dependency edges to prior items (typed: why the item
    /// references the target).
    #[serde(default)]
    pub dependencies: Vec<DependencyEdge>,
    /// Typed labels: core content labels, lifecycle markers and extension
    /// namespaces. Promotion and GC decide membership by these instead of
    /// raw string matching.
    #[serde(default)]
    pub tags: Vec<crate::label::Label>,
    /// Model/operator-directed GC hint (`context.gc_hint`): while set, the
    /// item is treated as a root by every GC pass until a later directive
    /// clears it.
    #[serde(default)]
    pub keep_alive: bool,
    /// Model/operator-directed lease (`context.lease`): the item is treated
    /// as a GC root until this turn (inclusive); `None` means no lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until_turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// GC dimension: whether the item is in the model-visible heap or in the
    /// reversible eviction buffer. Set by the GC pass, not by the semantic
    /// residency machine.
    #[serde(default)]
    pub residency: ContextResidency,
    /// How many full GC passes the item survived without being a root. Root
    /// reachability resets it to 0; exceeding the configured cap makes an
    /// unmarked item an eviction candidate. The generational dimension of GC.
    #[serde(default)]
    pub gc_generation: u32,
    /// Tick of the GC pass that moved this item into the reversible eviction
    /// buffer. `None` while resident. Reactivation skips items evicted by
    /// the current pass, so an item cannot bounce out and back in one GC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_at_tick: Option<u64>,
    /// Precomputed entity signature of `content` (the same tokens the
    /// policy, dependency linking, supersession and GC all use), so the hot
    /// paths do not re-parse item content on every pass. The engine keeps it
    /// in sync; `serde(default)` keeps old checkpoints and hand-built items
    /// valid (restore backfills items with an empty signature).
    #[serde(default)]
    pub entities: Vec<String>,
    /// 工作区相对路径：live `fs.read` 从 ToolOutput.metadata.path 盖章，
    /// 不从带行号的 model_content 猜测。缺省兼容旧 checkpoint。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// 同一次读取的内容摘要（`fs.read` 的 SHA-256 hex revision）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_revision: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusState {
    pub task_id: TaskId,
    pub goal: String,
    pub current_query: String,
    pub phase: String,
    #[serde(default)]
    pub active_entities: Vec<String>,
    pub generation: u64,
}

impl FocusState {
    /// Build a focus for an *existing* task. The task id comes from the
    /// runtime's `TaskManager` — the context engine must never mint a
    /// `TaskId` (task identity is runtime-owned), which is why this is the
    /// only constructor.
    pub fn for_task(task_id: TaskId, goal: impl Into<String>) -> Self {
        let goal = goal.into();
        Self {
            task_id,
            current_query: goal.clone(),
            goal,
            phase: "working".to_string(),
            active_entities: Vec::new(),
            generation: 0,
        }
    }
}

/// Runtime scope entity: a container that owns items for one period of
/// execution. Scopes form a tree — Session -> Task -> Focus -> Tool — and
/// each carries its own lifecycle (open/active/suspended/closed). Closing a
/// scope promotes its durable outcomes to the parent and releases the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeKind {
    Session,
    Task,
    Focus,
    Tool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScopeState {
    Open,
    Active,
    Suspended,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    pub id: ScopeId,
    pub parent: Option<ScopeId>,
    pub kind: ScopeKind,
    pub state: ScopeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub opened_tick: u64,
    pub last_active_tick: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_tick: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextIngress {
    UserMessage {
        content: String,
    },
    AssistantMessage {
        content: String,
    },
    ToolObservation {
        output: ToolOutput,
        /// The tool scope the observation belongs to, opened by the runtime
        /// at tool start. `None` when the caller has no scope tracking
        /// (replay, standalone use).
        #[serde(default)]
        scope_id: Option<ScopeId>,
    },
    /// A lightweight, bounded mid-turn working-set signal from the runtime.
    /// Heating consumes `resources` (trusted path@revision touches) only.
    /// `content` is a legacy prose field: old JSON still deserializes, but
    /// engines must not extract hot entities from it. No item is created.
    WorkingSetSignal {
        #[serde(default)]
        resources: Vec<ResourceTouch>,
        /// Legacy prose. Ignored for heating.
        #[serde(default)]
        content: String,
    },
    /// A structured context directive from a tool's output (gc hint, tag,
    /// lease). Tools never touch the engine — the runtime routes the
    /// directive here and the engine applies it or ignores it when the
    /// target item is gone.
    ContextDirective {
        action: ContextAction,
    },
    FocusChanged {
        focus: FocusState,
    },
    /// The current focus is cleared: the active task is suspended (not
    /// completed), its scopes stay open for a later resume, and subsequent
    /// turns run without a task until a `FocusChanged` reactivates one.
    FocusCleared,
    Pin {
        content: String,
        kind: ContextKind,
    },
    TaskCompleted {
        task_id: Option<TaskId>,
        summary: String,
    },
}

/// A structured context directive a tool may attach to its output as a
/// `RuntimeDirective` (context control). The runtime routes it to the
/// context engine; the engine applies it or silently ignores it when the
/// target item is gone. Every directive must be explainable in the
/// lifecycle ledger: "item kept alive because ...", "item leased until
/// turn N ...", "item tagged because ...".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContextAction {
    /// Keep an item resident across GC passes until a later directive
    /// clears the hint (`context.gc_hint`).
    GcHint {
        item_id: ContextItemId,
        keep_alive: bool,
    },
    /// Attach an extension tag to an item (`context.tag`).
    Tag { item_id: ContextItemId, tag: String },
    /// Protect an item from GC for the next `turns` turns
    /// (`context.lease`).
    Lease { item_id: ContextItemId, turns: u32 },
    /// Run a full GC pass now (`context.collect`). Handled by the runtime
    /// — it owns the GC pass — not by `ContextIngress::ContextDirective`.
    Collect,
    /// Bring an item back into the working set under its *original* item
    /// id (`context.admit`). Unlike `fetch` (transient read only), admit
    /// produces one lifecycle transition: the item re-enters the heap with
    /// the same id, re-stamped into the current task's scope, so it can be
    /// materialized without a later re-fetch. Terminal items (superseded,
    /// verified-fixed, tombstoned) are refused — semantic death never
    /// resurrects.
    Admit {
        item_id: ContextItemId,
        reason: String,
    },
    /// Persist a fact derived from a ref (`context.derive`): a *new* item
    /// with a new id and an explicit `DerivedFrom` edge to the source ref,
    /// so the derived knowledge is traceable but never confuses the source
    /// ref's identity with a copy.
    Derive {
        item_id: ContextItemId,
        fact: String,
        reason: String,
    },
    /// Replace the whole anchor-root projection (`task.anchor` roots →
    /// context policy). The runtime pushes this whenever the active task's
    /// anchor moves (patch/update/focus change) and before a GC pass, so
    /// GC and materialization see the current root set without the engine
    /// ever owning task authority. Not per-claim add/remove: the anchor is
    /// CAS authority, and this is a bounded replacement of its projection.
    AnchorRoots { roots: Vec<AnchorRootClaim> },
    /// Replace the TaskProgress checked-file projection (`path` /
    /// `path@revision` rows). The runtime pushes this before a GC pass so
    /// file-body auto-reactivation can skip paths the operational cache
    /// already names. Not a model-facing `context.manage` op; not P3.
    CheckedFiles { files: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ContextMaintenanceTrigger {
    #[default]
    UserInput,
    BeforeModel,
    AfterModel,
    AfterTool,
    FocusChanged,
    TaskCompleted,
    Checkpoint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextQuery {
    pub current_input: String,
    pub budget_tokens: usize,
    #[serde(default)]
    pub hints: ContextHints,
}

/// Runtime knobs for one materialization. Kept open so later policy work can
/// add per-request guidance without breaking the contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextHints {
    /// Hard cap on how many items the engine may select. Dependency
    /// expansion still respects it; `None` means the budget alone decides.
    pub max_selected_items: Option<usize>,
    /// The active task's typed root claims, projected from its TaskAnchor
    /// by the runtime. `PromptRequired` claims force their item into the
    /// model frame; the engine never sees the anchor itself (task authority
    /// stays with the TaskManager). Bounded by `MAX_ANCHOR_ROOT_CLAIMS`;
    /// terminal semantic state is never resurrected by a claim.
    #[serde(default)]
    pub anchor_roots: Vec<AnchorRootClaim>,
    /// Bounded prompt projection of the active TaskAnchor. The engine must
    /// not copy this onto `MaterializedContext` for prompt rendering; the
    /// runtime assembler receives the view from TaskManager. Engines may
    /// ignore it.
    #[serde(default)]
    pub task: Option<TaskAnchorView>,
    /// Checked `path` / `path@revision` rows from the runtime TaskProgress
    /// projection. Engines may price historical file-body items as
    /// descriptors when a row covers the path. They must not copy this
    /// onto `MaterializedContext` for prompt rendering.
    #[serde(default)]
    pub checked_files: Vec<String>,
    /// Current-directive exact-mention ∩ ExecutionState known paths.
    /// Engines may transiently project those file bodies for this request
    /// without changing residency (no Warm→Resident, no Stored Admit).
    #[serde(default)]
    pub foreground_resources: Vec<ResourceKey>,
}

/// Workspace path identity used by prompt hints. Not a GC residency key.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResourceKey {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Hard cap on the anchor-root projection the runtime pushes into one
/// materialization or GC pass. The model cannot grow the root set without
/// bound through anchor patches.
pub const MAX_ANCHOR_ROOT_CLAIMS: usize = 64;

/// Hard cap on the checked-file projection the runtime pushes into one
/// GC pass. Matches the ResumePoint file cache; extra rows are dropped
/// from the front (oldest) so the engine never sees an unbounded set.
pub const MAX_CHECKED_FILE_HINTS: usize = 32;

/// Max file bodies in one CURRENT FOREGROUND EVIDENCE projection.
pub const MAX_FOREGROUND_RESOURCES: usize = 2;

/// Token cap for the combined foreground bodies of one model request.
pub const MAX_FOREGROUND_TOKENS: usize = 2048;

/// Bounded prompt projection of a `TaskAnchor`. Raw refs and bodies stay
/// out: the assembler renders this contract in the focus frame, while
/// `anchor_roots` carry the independent prompt/residency/storage claims.
/// The engine must not own, score, or patch this view.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TaskAnchorView {
    pub revision: u64,
    pub original_goal: String,
    pub current_interpretation: String,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub plan_progress: Vec<String>,
    pub open_loops: Vec<String>,
}

impl TaskAnchorView {
    pub fn is_empty(&self) -> bool {
        self.original_goal.is_empty()
            && self.current_interpretation.is_empty()
            && self.constraints.is_empty()
            && self.acceptance_criteria.is_empty()
            && self.plan_progress.is_empty()
            && self.open_loops.is_empty()
    }
}

/// Bounded prompt projection of an `ExecutionState`. Operational cache only:
/// checked resources, revision-bound verification facts, failed operations.
/// Goal/blockers/next-actions belong to `TaskAnchor`. Bodies stay in storage.
/// Prompt projection is hard-capped by [`MAX_TASK_PROGRESS_PROMPT_CHARS`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TaskProgressView {
    pub anchor_revision: u64,
    #[serde(default)]
    pub workspace_revision: u64,
    pub checked_files: Vec<String>,
    pub verifications: Vec<String>,
    pub failed_commands: Vec<String>,
    /// Deterministic stall signal (MOD-PROG-01): the same operation
    /// signature has produced no world progress for consecutive rounds.
    /// Advisory prompt line, never an execution block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_warning: Option<String>,
}

/// Hard cap on the assembled TASK PROGRESS prompt block. List-length caps
/// on ResumePoint are not enough: 32 × 200-char rows still overflow.
pub const MAX_TASK_PROGRESS_PROMPT_CHARS: usize = 2_048;

impl TaskProgressView {
    pub fn is_empty(&self) -> bool {
        self.checked_files.is_empty()
            && self.verifications.is_empty()
            && self.failed_commands.is_empty()
    }

    /// Whether `Checked` already names this workspace path (`path` or
    /// `path@revision`). Slash-normalized; a prefix cousin (`src/a.rs.bak`)
    /// is not a hit.
    pub fn covers_path(&self, path: &str) -> bool {
        checked_files_cover_path(&self.checked_files, path)
    }
}

/// Whether a TaskProgress `Checked` row already names `path` (`path` or
/// `path@revision`). Shared by the assembler and engine packing.
pub fn checked_files_cover_path(checked_files: &[String], path: &str) -> bool {
    let path = crate::normalize_resource_path(path);
    if path.is_empty() {
        return false;
    }
    let prefix = format!("{path}@");
    checked_files.iter().any(|row| {
        let row = crate::normalize_resource_path(row);
        row == path || row.starts_with(&prefix)
    })
}

/// Why a record is a root. Independent of `AnchorRootStrength` (how strongly
/// to hold it) so prompt, residency, and storage decisions stay separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootReason {
    #[default]
    TaskAnchor,
    CurrentEpisode,
    OpenLoop,
    HardConstraint,
    ActiveError,
    CompletionPending,
    CompletionEvidence,
    StrongDependency,
    ExplicitLease,
    AuditPin,
}

/// One typed root claim projected from a `TaskAnchor` into the context
/// policy. The ref names a context item id, `context://run/<id>` uri, or an
/// exact entity signature; the strength says how strongly the policy must
/// hold it. The anchor itself lives with the runtime — this is a bounded
/// projection, never a copy of task authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AnchorRootClaim {
    /// The referenced item: an item id string, a `context://run/<id>` uri,
    /// or an exact entity signature.
    pub item_ref: String,
    /// How strongly the context policy must hold the claim.
    pub strength: AnchorRootStrength,
    /// Which anchor field the claim came from (provenance/audit).
    pub source_field_id: String,
    /// Anchor revision this claim was projected from.
    #[serde(default)]
    pub anchor_revision: u64,
    /// Why this claim is a root. Independent of `strength`.
    #[serde(default)]
    pub reason: RootReason,
}

impl Default for AnchorRootClaim {
    fn default() -> Self {
        Self {
            item_ref: String::new(),
            strength: AnchorRootStrength::Recallable,
            source_field_id: String::new(),
            anchor_revision: 0,
            reason: RootReason::TaskAnchor,
        }
    }
}

/// How strongly the context policy must hold one anchor root claim.
/// The three protections are independent decisions: prompt membership,
/// online residency, and storage retention. PromptRequired implies
/// residency only because a body must be available to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorRootStrength {
    /// The item must be in the model prompt for this task.
    PromptRequired,
    /// The item must stay resident in the working set (a GC root).
    ResidentRequired,
    /// The item must survive in storage (never permanently deleted).
    StorageRequired,
    /// The item is recallable on demand; no residency guarantee.
    Recallable,
}

impl AnchorRootStrength {
    /// Mandatory materialization: force the item into the next model frame.
    pub fn requires_prompt(self) -> bool {
        matches!(self, Self::PromptRequired)
    }

    /// Online residency: keep or recall the body in the fast working set.
    /// PromptRequired implies residency because rendering needs the body.
    pub fn requires_residency(self) -> bool {
        matches!(self, Self::PromptRequired | Self::ResidentRequired)
    }

    /// Storage retention: forbid permanent deletion while the claim stands.
    /// Does not by itself keep the body resident or in the prompt.
    pub fn requires_storage(self) -> bool {
        matches!(self, Self::StorageRequired)
    }
}

/// One explainable root protection applied this GC/storage-GC pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AnchorRootProtection {
    pub item_ref: String,
    pub strength: AnchorRootStrength,
    pub source_field_id: String,
    pub anchor_revision: u64,
    pub reason: RootReason,
}

impl From<&AnchorRootClaim> for AnchorRootProtection {
    fn from(claim: &AnchorRootClaim) -> Self {
        Self {
            item_ref: claim.item_ref.clone(),
            strength: claim.strength,
            source_field_id: claim.source_field_id.clone(),
            anchor_revision: claim.anchor_revision,
            reason: claim.reason,
        }
    }
}

/// Hard bound on full working-set item ids carried by one consumption
/// acknowledgement. The runtime applies the same cap to materialization, so
/// the audit/event payload cannot grow with the history length.
pub const CONTEXT_CONSUMPTION_ACK_ITEM_CAP: usize = 256;

/// Exact, bounded acknowledgement of the context frame a successful model
/// operation consumed. `materialize` is a non-consuming preview; only this
/// commit may reinforce access/recency. Refused, failed, cancelled or stale
/// operations never send an acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextConsumptionAck {
    pub turn_id: TurnId,
    pub operation_id: OperationId,
    pub model_round: usize,
    /// Opaque id returned by the exact `materialize` preview used to build
    /// the provider request.
    pub materialization_id: u64,
    /// Full context bodies actually rendered into the final request.
    pub item_ids: Vec<ContextItemId>,
    /// Lightweight external descriptors actually rendered into the final
    /// request. These are ids, not copied summaries/bodies.
    #[serde(default)]
    pub external_item_ids: Vec<ContextItemId>,
    /// Foreground evidence bodies the prompt actually rendered
    /// (`MaterializedContext.foreground`): transient rehydration the model
    /// saw even though it never changed residency. Observability only —
    /// engines record it as a weak access signal and must not Admit,
    /// reinforce scoring, or change residency from it.
    #[serde(default)]
    pub foreground_item_ids: Vec<ContextItemId>,
}

impl ContextConsumptionAck {
    pub fn validate(&self) -> AgentResult<()> {
        if self.item_ids.len() > CONTEXT_CONSUMPTION_ACK_ITEM_CAP {
            return Err(AgentError::InvalidRequest(format!(
                "context consumption ack carries {} item ids, above the {} cap",
                self.item_ids.len(),
                CONTEXT_CONSUMPTION_ACK_ITEM_CAP
            )));
        }
        if self.external_item_ids.len() > CONTEXT_MAP_VIEW_CAP {
            return Err(AgentError::InvalidRequest(format!(
                "context consumption ack carries {} external ids, above the {} cap",
                self.external_item_ids.len(),
                CONTEXT_MAP_VIEW_CAP
            )));
        }
        let mut seen = HashSet::with_capacity(self.item_ids.len() + self.external_item_ids.len());
        if self.item_ids.iter().any(|id| !seen.insert(*id)) {
            return Err(AgentError::InvalidRequest(
                "context consumption ack contains duplicate item ids".into(),
            ));
        }
        if self.external_item_ids.iter().any(|id| !seen.insert(*id)) {
            return Err(AgentError::InvalidRequest(
                "context consumption ack contains a duplicate item identity".into(),
            ));
        }
        // Foreground identities only need distinctness among themselves:
        // the same body may legitimately be both selected and foreground
        // rehydrated in one frame, and that overlap is not an error.
        let mut foreground_seen = HashSet::with_capacity(self.foreground_item_ids.len());
        if self
            .foreground_item_ids
            .iter()
            .any(|id| !foreground_seen.insert(*id))
        {
            return Err(AgentError::InvalidRequest(
                "context consumption ack contains duplicate foreground identities".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub importance: f32,
    pub focus_match: f32,
    pub recency: f32,
    pub access: f32,
    pub scope_bonus: f32,
    pub retention_bonus: f32,
    /// Reward for an item whose entities are hot in the current working
    /// set (user message + recent structured resource touches).
    #[serde(default)]
    pub entity_affinity: f32,
    pub total: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextSelection {
    pub item_id: ContextItemId,
    pub score: f32,
    pub approx_tokens: usize,
    pub reason: String,
    #[serde(default)]
    pub breakdown: ScoreBreakdown,
    /// Structured kind of the selected item. Measurement only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ContextKind>,
    /// Producer / source stamp (`user`, `tool:fs.read`, …). Measurement only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// True when this id has a reactivation trace this engine segment.
    #[serde(default)]
    pub reactivated: bool,
}

/// A single observed lifecycle state transition produced by one maintenance pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextStateTransition {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub from: AttentionState,
    pub to: AttentionState,
    pub turn: u64,
    pub reason: String,
}

/// Which lifecycle dimension a ledger row changed. The three orthogonal GC
/// dimensions (attention, semantic, residency) plus the GC pass itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleAxis {
    Attention,
    Semantic,
    Residency,
    Gc,
}

/// One row of the artifact-backed lifecycle ledger: a single item's
/// transition on one axis, with the cause and the clock it happened on.
/// Written into a bounded in-engine buffer and exported as a JSONL artifact
/// on demand (export is never on the context hot path). Every row answers
/// one of the acceptance questions: entered / selected / cooled-archived /
/// evicted-reactivated / consumed because of this, at turn N, triggered by X.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextLifecycleRecord {
    pub item_id: ContextItemId,
    /// Per-item revision: how many ledger rows this item has accumulated.
    pub revision: u64,
    pub axis: LifecycleAxis,
    /// State the item left ("" for the first row on an axis).
    pub from: String,
    /// State the item entered.
    pub to: String,
    /// Why (mirrors the transition/eviction reason).
    pub cause: String,
    /// What triggered the change: a maintenance trigger name, "gc",
    /// "scope_close", "directive" or "ingest".
    pub trigger: String,
    /// User turn the change happened in.
    pub turn: u64,
    /// Related item when the change references one (superseded-by,
    /// derived-from, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related_id: Option<ContextItemId>,
    /// Event-sequence clock value at the change (orders the ledger).
    pub event_seq: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextDiagnostics {
    pub total_items: usize,
    pub active_items: usize,
    pub cooling_items: usize,
    pub archived_items: usize,
    /// Items whose semantic lifecycle ended (TTL/staleness): permanently
    /// dead, never resurrected by Context GC.
    #[serde(default)]
    pub tombstoned_items: usize,
    pub approx_active_tokens: usize,
    pub focus_generation: u64,
    /// Engine-owned focused task. Used for restore alignment. Production
    /// engines leave `MaterializedContext.focus` empty; this is the
    /// implementation-agnostic read of engine focus authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_task_id: Option<TaskId>,
    #[serde(default)]
    pub turn: u64,
    /// Monotonic event-sequence clock: advances on every state-changing
    /// engine operation (ingest/maintain/GC/ack/scope ops), never on
    /// materialize. TTL rules name their clock explicitly; this one orders
    /// events and measures event-distance, not age.
    #[serde(default)]
    pub event_seq: u64,
    #[serde(default)]
    pub tool_round: u64,
    #[serde(default)]
    pub open_scopes: usize,
    #[serde(default)]
    pub active_scopes: usize,
    #[serde(default)]
    pub suspended_scopes: usize,
    #[serde(default)]
    pub closed_scopes: usize,
    /// GC dimension: items currently in the model-visible heap.
    #[serde(default)]
    pub resident_items: usize,
    /// UTF-8 bytes of Resident heap bodies. Item counts hide large vs small
    /// entries; this is the boundedness axis prompt size cannot substitute.
    #[serde(default)]
    pub resident_bytes: usize,
    /// GC dimension: items sitting in the reversible eviction buffer.
    #[serde(default)]
    pub warm_items: usize,
    /// GC dimension: items externalized to the context store, still tracked
    /// in memory with a lightweight entry (`Cold`, content in the store).
    #[serde(default)]
    pub cold_items: usize,
    /// GC dimension: externalized entries that aged out of the working set;
    /// only the store retains them (`External`).
    #[serde(default)]
    pub external_items: usize,
    /// Cumulative evictions / reactivations / externalizations since the
    /// engine started, so a run's GC behavior is explainable without
    /// replaying every report.
    #[serde(default)]
    pub gc_evicted_total: u64,
    #[serde(default)]
    pub gc_reactivated_total: u64,
    #[serde(default)]
    pub gc_externalized_total: u64,
    #[serde(default)]
    pub gc_storage_deleted_total: u64,
    /// Cumulative graded retrieval stamps this run (`CTX-GC-11` / M15).
    /// Search-hit is the weakest; consumption ack the strongest. These
    /// count applied stamps, not descriptor rows returned.
    #[serde(default)]
    pub access_search_hits: u64,
    #[serde(default)]
    pub access_inspects: u64,
    #[serde(default)]
    pub access_fetches: u64,
    #[serde(default)]
    pub access_admits: u64,
    #[serde(default)]
    pub access_consumption_acks: u64,
    /// Foreground bodies consumed by successful model rounds (weak
    /// observational signal; never reinforces access or changes residency).
    #[serde(default)]
    pub foreground_consumed_acks: u64,
    /// Hot-reactivation utility: reactivated items later selected into a
    /// materialized frame. Measurement only; does not change GC policy.
    #[serde(default)]
    pub reactivation_selected: u64,
    #[serde(default)]
    pub reactivation_consumed: u64,
    #[serde(default)]
    pub reactivation_selected_tokens: u64,
    #[serde(default)]
    pub reactivation_consumed_tokens: u64,
    /// Engine-local reactivation *events* this segment (a later GC pass on
    /// the same id counts again). Zeroed on restore; eval sums `ContextGc`
    /// events for the run-global count.
    #[serde(default)]
    pub reactivation_events: u64,
    /// Distinct ids that entered a reactivation trace this engine segment.
    /// Zeroed on restore; eval unions `ContextGc` reactivation ids.
    #[serde(default)]
    pub unique_reactivated: u64,
    #[serde(default)]
    pub reactivated_tokens: u64,
    #[serde(default)]
    pub reactivation_tool_observation_selected: u64,
    #[serde(default)]
    pub reactivation_tool_observation_consumed: u64,
    #[serde(default)]
    pub reactivation_file_observation_selected: u64,
    #[serde(default)]
    pub reactivation_file_observation_consumed: u64,
    /// 有界压缩器（B 折叠 / C 派生）累计的 provider 输入 token。
    #[serde(default)]
    pub compaction_input_tokens: u64,
    /// 有界压缩器累计的 provider 输出 token。
    #[serde(default)]
    pub compaction_output_tokens: u64,
    /// Cumulative `fs.read` classifications this engine segment.
    /// `warm` + `stored` approximate GC-caused rereads; `previously-selected`
    /// is body-in-last-prompt only (not descriptorized `path@rev`).
    #[serde(default)]
    pub reread_previously_selected: u64,
    #[serde(default)]
    pub reread_selected_descriptor: u64,
    #[serde(default)]
    pub reread_external_descriptor: u64,
    #[serde(default)]
    pub reread_resident_unselected: u64,
    #[serde(default)]
    pub reread_warm: u64,
    #[serde(default)]
    pub reread_stored: u64,
    #[serde(default)]
    pub reread_first_read: u64,
}

/// One structured entry of the materialized working set. The engine returns
/// content, never rendered prompt text; the runtime's prompt assembler
/// decides how the entry is presented to the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedItem {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub attention: AttentionState,
    #[serde(default)]
    pub semantic: SemanticState,
    /// Retention is exposed so the runtime's final budget guard can keep
    /// pinned items while trimming the context frame, and so the model can
    /// see which entries are durable.
    #[serde(default = "default_retention")]
    pub retention: ContextRetention,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 选中条目若是文件正文观察，装配器用它标路径，不从 content 猜。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Stamped content revision for `file_path`. Prompt identity is
    /// `path@revision`; the assembler must not parse it out of `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_revision: Option<String>,
}

/// Missing `retention` on old wire/checkpoint data means a normal working
/// item, not a pinned one.
fn default_retention() -> ContextRetention {
    ContextRetention::Working
}

/// The structured result of one `ContextEngine::materialize` call: the
/// selected historical working set, the lightweight external context map
/// (externalized items visible only by `ContextRef`) and the
/// selections/diagnostics. Prompt rendering is the runtime assembler's job.
/// `focus` / `task` remain on the wire for older snapshots but engines must
/// leave them empty — CURRENT FOCUS and TaskAnchor are runtime-owned.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MaterializedContext {
    /// Opaque identity of this preview. It is echoed by
    /// `ContextConsumptionAck` only after the final provider request succeeds.
    #[serde(default)]
    pub materialization_id: u64,
    /// Engine-internal snapshot leftover. PromptAssembler must not read this;
    /// production engines leave it empty.
    pub focus: Option<FocusState>,
    /// Engine-internal snapshot leftover. PromptAssembler must not read this;
    /// production engines leave it empty.
    #[serde(default)]
    pub task: Option<TaskAnchorView>,
    pub items: Vec<MaterializedItem>,
    /// The lightweight context map: externalized items the model can only
    /// see as references (`context://...`), never as full content. The
    /// view is bounded by [`CONTEXT_MAP_VIEW_CAP`].
    #[serde(default)]
    pub external: ContextMapView,
    pub selected: Vec<ContextSelection>,
    pub approx_tokens: usize,
    pub diagnostics: ContextDiagnostics,
    /// Transient CURRENT FOREGROUND EVIDENCE bodies. Not a residency
    /// change: Warm stays Warm, Stored is not Admitted, and consumption
    /// ack must not stamp these ids.
    #[serde(default)]
    pub foreground: Vec<MaterializedItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextMaintenanceReport {
    pub promoted: usize,
    pub cooled: usize,
    pub archived: usize,
    /// Items whose semantic lifecycle ended this pass (TTL/staleness).
    #[serde(default)]
    pub tombstoned: usize,
    #[serde(default)]
    pub turn: u64,
    #[serde(default)]
    pub transitions: Vec<ContextStateTransition>,
    pub diagnostics: ContextDiagnostics,
    /// 本轮维护里压缩器花费的 provider 输入 token（脚本化实现为 0）。
    #[serde(default)]
    pub compaction_input_tokens: u64,
    /// 本轮维护里压缩器花费的 provider 输出 token。
    #[serde(default)]
    pub compaction_output_tokens: u64,
    /// Explicit compaction passes drained this maintain (ingest-time episode
    /// rotation plus in-pass folds). Eval sums [`crate::RuntimeEvent::ContextCompacted`].
    #[serde(default)]
    pub compactions: Vec<ContextCompaction>,
}

/// Why a bounded compaction pass ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    EpisodeRotation,
    TaskCompleted,
    RollingFold,
}

/// One bounded compaction pass. Token fields are the compressor call itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompaction {
    pub reason: CompactionReason,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub source_items: usize,
}

/// One reversible eviction produced by a full GC pass: the item left the
/// model-visible heap for the bounded eviction buffer, with the reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEviction {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    /// How many GC passes the item survived without being a root.
    pub generation: u32,
    pub evicted_at_tick: u64,
    /// Why this item was evicted (not reachable from roots, semantically
    /// dropped, stale, ...) — every eviction must be explainable.
    pub reason: String,
}

/// One reactivation produced by a full GC pass: an evicted item became
/// relevant again (hot entities, focus match, pin, task) and re-entered the
/// heap, with the reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextReactivation {
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub reactivated_at_tick: u64,
    /// Why this item came back — every reactivation must be explainable.
    pub reason: String,
}

/// The result of one full `ContextEngine::gc` pass: mark roots, sweep
/// unmarked items into the reversible eviction buffer, reactivate items
/// that became relevant again. Context GC never deletes information: buffer
/// overflow *externalizes* items to the context store (`Cold`), and only
/// the separate, conservative Storage GC may delete store files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextGcReport {
    /// Items marked as roots this pass (pins, active scopes, hot entities,
    /// reachable dependencies).
    pub marked_roots: usize,
    /// Items left in the model-visible heap after the pass.
    pub resident: usize,
    /// Items moved to the reversible eviction buffer this pass.
    pub evicted: usize,
    /// Items moved back from the buffer into the heap this pass.
    pub reactivated: usize,
    /// Items moved from the buffer to the external context store this pass
    /// (buffer overflow). Their content is not deleted — it lives on under
    /// a `ContextRef`, visible through the lightweight context map.
    #[serde(default)]
    pub externalized: usize,
    /// Item ids externalized this pass. Bounded by `externalized`; used by
    /// M15 found-after-forgotten accounting. Empty on pre-field reports.
    #[serde(default)]
    pub externalized_ids: Vec<ContextItemId>,
    /// Warm -> Cold aging in the other direction: externalized entries that
    /// became `External` this pass (only the store retains them).
    #[serde(default)]
    pub aged_external: usize,
    /// Resident items this pass kept in the heap by an anchor root claim
    /// (the working set slice the active task's TaskAnchor protects).
    #[serde(default)]
    pub anchor_roots_protected: usize,
    /// Bounded per-claim explanations for those protections
    /// (`anchor_revision + source_field + RootReason`).
    #[serde(default)]
    pub anchor_root_protections: Vec<AnchorRootProtection>,
    #[serde(default)]
    pub evictions: Vec<ContextEviction>,
    #[serde(default)]
    pub reactivations: Vec<ContextReactivation>,
    /// Store blob deletions that failed after a successful recall commit
    /// (real filesystem errors). The recalled content is resident, so this
    /// never loses information — the leftover blob is re-owned or deleted
    /// by the next startup reconcile.
    #[serde(default)]
    pub store_blob_delete_errors: usize,
    /// Bytes written to the external context store this pass (full
    /// externalization bodies). Store I/O accounting for the M15 baseline.
    #[serde(default)]
    pub store_write_bytes: u64,
    /// Bytes read back from the external context store this pass (recall
    /// bodies).
    #[serde(default)]
    pub store_read_bytes: u64,
    /// Items recalled from the external context store this pass
    /// (full-body reads).
    #[serde(default)]
    pub store_recalled_items: u64,
    pub diagnostics: ContextDiagnostics,
}

/// A reference to an externalized item in the context store. The model only
/// ever sees these lightweight references for `Cold`/`External` items, never
/// their full content — reading an externalized item back is a deliberate
/// operation (e.g. a future context tool), not a passive materialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextRef {
    /// Stable, addressable uri, e.g. `context://run/<item-id>`.
    pub uri: String,
    pub item_id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    /// Bounded summary (content truncated at externalization time).
    pub summary: String,
    pub created_tick: u64,
}

/// One entry of the external context map: a lightweight record of an item
/// whose content lives in the context store. `Cold` entries can still be
/// recalled by hot-entity matches (the store is read back); `External`
/// entries only exist as references until a future context tool pulls them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalizedContext {
    pub item_id: ContextItemId,
    /// Task the item belonged to when it was externalized, kept so
    /// deterministic retrieval can filter by task without reading the file.
    #[serde(default)]
    pub task_id: Option<TaskId>,
    /// Scope the item belonged to when it was externalized. Kept so a
    /// scope close can re-stamp the membership of external entries exactly
    /// like resident and warm bodies — the scope-close promotion must not
    /// lose items whose content already left the engine. `None` for
    /// entries externalized before the stamp existed (or by runtimes that
    /// do not use scopes); task closes fall back to `task_id`.
    #[serde(default)]
    pub scope_id: Option<ScopeId>,
    pub kind: ContextKind,
    pub scope: ContextScope,
    pub retention: ContextRetention,
    pub attention: AttentionState,
    pub semantic: SemanticState,
    pub context_ref: ContextRef,
    pub externalized_at_tick: u64,
    pub last_access_tick: u64,
    pub residency: ContextResidency,
    /// Entity signature captured at externalize time. Recall filters on this
    /// in memory first, so a Cold-recall pass reads only the store files
    /// whose entities actually match — not every externalized entry.
    #[serde(default)]
    pub entities: Vec<String>,
    /// Tags captured at externalize time, so the materialized external view
    /// and future context retrieval can rank by open-loop/decision labels
    /// without reading the store file.
    #[serde(default)]
    pub tags: Vec<Label>,
    /// Dependency edges captured at externalize time, so the Storage GC can
    /// run a reachability closure over resident *and* external entries
    /// instead of checking only single incoming edges from the heap.
    #[serde(default)]
    pub dependencies: Vec<DependencyEdge>,
    /// Model/operator-directed protection captured at externalize time.
    /// A protected item is normally a GC root and never leaves the heap, so
    /// these are almost always the default; they exist so stored metadata
    /// can *represent* every directive field (a protection survives a
    /// buffer-overflow externalize of a hint that arrived just before the
    /// pass, and a completed task clears them in every body location).
    #[serde(default)]
    pub keep_alive: bool,
    #[serde(default)]
    pub lease_until_turn: Option<u64>,
    /// The `State::gc_epoch` at which this entry was last accessed. Aging
    /// Cold -> External compares *generations* (only full GC increments the
    /// epoch), never ticks — ingest/maintain/materialize also advance the
    /// tick counter and would make a pass-based TTL meaningless. `None` for
    /// entries restored from pre-epoch checkpoints: they are treated as
    /// accessed at the current epoch instead of aging out instantly.
    #[serde(default)]
    pub last_access_gc_epoch: Option<u64>,
    /// Checksum of the stored blob this entry owns, captured at write time.
    /// The startup reconcile validates blobs against it; the hot read path
    /// (fetch) skips the hash so per-item retrieval stays IO-cheap. `None`
    /// for entries written before checksums existed (restore reconciles
    /// them by parsing + id match).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_checksum: Option<String>,
    /// Source authority captured at externalize time: which producer
    /// (user/tool/model/artifact) the item originally came from. Kept on
    /// the entry so `inspect` and the future fetch/admit authority checks
    /// can see where an externalized item came from without reading the
    /// store file. `None` for entries externalized before the field
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// 外部化时的打分权重（importance/relevance），随条目保留：inspect
    /// 能如实反映外部条目的候选价值，而不是固定 0.0。这是 ContextCatalog
    /// 统一权威元数据的前置——外部化不得把权威降级。
    #[serde(default)]
    pub importance: f32,
    #[serde(default)]
    pub relevance: f32,
    /// 真实创建时钟，随条目保留。inspect 排序据此用真实创建时间，
    /// 不再以 `externalized_at_tick` 近似。
    #[serde(default)]
    pub created_tick: u64,
    #[serde(default)]
    pub created_turn: u64,
    #[serde(default)]
    pub last_access_turn: u64,
    #[serde(default)]
    pub last_selected_turn: u64,
    #[serde(default)]
    pub access_count: u32,
    /// 最近一次刷新访问时钟的信号等级。弱信号不得覆盖强信号；旧
    /// checkpoint 缺省为 `None`。
    #[serde(default)]
    pub last_access_signal: AccessSignal,
    /// 自上次更强信号以来，search 已刷新 Cold 老化锚点的次数。饱和后
    /// search 只能动 ranking 时钟，不能再推迟 Cold -> External。
    #[serde(default)]
    pub search_reinforce_count: u32,
    /// 外部化时的 GC 世代，随条目保留（recall 后从原世代继续，而非清零）。
    #[serde(default)]
    pub gc_generation: u32,
    /// 进入 warm 缓冲的 tick，随条目保留。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evicted_at_tick: Option<u64>,
    /// 外部化时的文件身份，inspect/search 不必读 store body。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_revision: Option<String>,
}

/// Cap on the external refs surfaced in one materialized context. The
/// prompt renders refs only, so the bound is about prompt cost: the model
/// should see a handful of pullable refs, not the whole external history.
pub const CONTEXT_MAP_VIEW_CAP: usize = 32;

/// Default coding-agent system policy. Short on purpose: a stable runtime
/// contract, not a retrieval tutorial. Do not name ops, prefer/avoid lists,
/// or retune scoring from this text.
pub const DEFAULT_CODING_AGENT_SYSTEM_PROMPT: &str = concat!(
    "You are a focused coding agent. Work on the current task only. ",
    "The selected working context is not the full catalog; prior evidence may remain outside this frame and can be searched or retrieved with context tools. ",
    "Additional tools can be discovered and loaded with capability tools."
);

/// The bounded, model-facing view of the external context map. The engine
/// selects at most [`CONTEXT_MAP_VIEW_CAP`] refs per materialization; the
/// type enforces that bound, so a producer that forgets the cap fails
/// loudly at the type boundary instead of silently growing the prompt.
/// Newtype-serializes transparently as the inner refs, so the wire shape
/// is unchanged from the raw `Vec`.
#[derive(Debug, Clone, Default)]
pub struct ContextMapView(Vec<ExternalizedContext>);

impl ContextMapView {
    /// Build a view from a bounded selection. Panics when `entries`
    /// exceeds [`CONTEXT_MAP_VIEW_CAP`]: the engine's selection policy is
    /// the only local producer and is itself bounded, so an over-cap input
    /// is an internal invariant violation, not a runtime condition.
    pub fn new(entries: Vec<ExternalizedContext>) -> Self {
        assert!(
            entries.len() <= CONTEXT_MAP_VIEW_CAP,
            "external context map view of {} refs exceeds the cap of {CONTEXT_MAP_VIEW_CAP}",
            entries.len()
        );
        Self(entries)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ExternalizedContext> {
        self.0.iter()
    }

    pub fn as_slice(&self) -> &[ExternalizedContext] {
        &self.0
    }
}

impl std::ops::Deref for ContextMapView {
    type Target = [ExternalizedContext];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> IntoIterator for &'a ContextMapView {
    type Item = &'a ExternalizedContext;
    type IntoIter = std::slice::Iter<'a, ExternalizedContext>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Serialize for ContextMapView {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContextMapView {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let entries = Vec::<ExternalizedContext>::deserialize(deserializer)?;
        if entries.len() > CONTEXT_MAP_VIEW_CAP {
            return Err(serde::de::Error::custom(format!(
                "external context map view of {} refs exceeds the cap of {CONTEXT_MAP_VIEW_CAP}",
                entries.len()
            )));
        }
        Ok(Self(entries))
    }
}

/// Deterministic search over the catalog — no vectors.
/// Implemented dimensions: free-text over entity signatures, summary and
/// uri; kind/scope/task/label filters; recency-aware ranking. Hits may be
/// Resident, Warm, Cold, or External. Candidate generation uses the
/// engine's `ContextCatalog` indexes (not a full-history scan) when a
/// filter or an entity/label key can bound the set.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextSearchQuery {
    /// Free-text query matched (case-insensitively) against entity
    /// signatures, the entry summary and the ref uri. Multi-word queries
    /// verify when every token (shared `tokenize` rule) appears in one
    /// entry's matchable text; the whole-needle substring stays the
    /// primary, ranking-privileged match.
    pub query: String,
    /// Optional kind filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ContextKind>,
    /// Optional scope filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ContextScope>,
    /// Optional task filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// Optional label filter, matched case-insensitively against
    /// `ExternalizedContext::tags` (`Label::as_str`). This is a real
    /// catalog index dimension, not a residual scan predicate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Cap on returned refs. `0` means the engine default (16).
    pub limit: usize,
}

impl ContextSearchQuery {
    pub fn new(query: impl Into<String>, limit: usize) -> Self {
        Self {
            query: query.into(),
            kind: None,
            scope: None,
            task_id: None,
            label: None,
            limit,
        }
    }
}

/// The result of one `ContextEngine::storage_gc` pass. Storage GC is the
/// only place information is permanently deleted, and it is deliberately
/// conservative: nothing is deleted while a resident/warm item still
/// depends on it, pinned/durable items are never touched, and only items
/// whose semantic lifecycle ended are candidates.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StorageGcReport {
    pub scanned: usize,
    pub deleted: usize,
    /// Entries kept this pass because an anchor root claim (`StorageRequired`)
    /// protects them — the active task's evidence must never be permanently
    /// deleted while the claim stands.
    #[serde(default)]
    pub anchor_roots_protected: usize,
    /// Bounded per-claim explanations for those storage protections.
    #[serde(default)]
    pub anchor_root_protections: Vec<AnchorRootProtection>,
    /// Store entries that could not be touched because the filesystem
    /// returned a real error (permission, disk). Those entries are *kept* —
    /// an IO failure must never be mistaken for "the file is already gone".
    #[serde(default)]
    pub io_errors: usize,
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// The outcome of a store reconcile pass: the on-disk blob directory is
/// brought back in line with the external map, so every formal blob has
/// exactly one owner and every stored record has one readable blob.
/// Uncertain state is quarantined (moved aside, evidence preserved), never
/// silently ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoreReconcileReport {
    /// Formal blobs (`<id>.json`) inspected.
    pub scanned: usize,
    /// Orphan blobs rebuilt into external-map entries (the file was valid
    /// and nothing else owned the id) — the conservative choice: context
    /// GC never purges, so a reachable file becomes a reference again.
    #[serde(default)]
    pub rebuilt: usize,
    /// Orphan blobs deleted because the same id was already live in the
    /// heap or warm buffer (the file was a stale duplicate of resident
    /// content).
    #[serde(default)]
    pub deleted_stale: usize,
    /// Unreadable / id-mismatched / checksum-mismatched blobs moved to the
    /// `quarantine/` subdirectory instead of being treated as deleted.
    #[serde(default)]
    pub quarantined: usize,
    /// Abandoned temp files (`*.tmp` from an interrupted atomic write)
    /// removed.
    #[serde(default)]
    pub temp_cleaned: usize,
    /// Real filesystem errors (permission, disk): the blob was left in
    /// place and surfaced, not guessed at.
    #[serde(default)]
    pub io_errors: usize,
    #[serde(default)]
    pub reasons: Vec<String>,
}

/// A bounded, UI/replay-friendly projection of one context item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItemSummary {
    pub id: ContextItemId,
    pub kind: ContextKind,
    pub scope: ContextScope,
    #[serde(default)]
    pub scope_id: Option<ScopeId>,
    pub attention: AttentionState,
    #[serde(default)]
    pub semantic: SemanticState,
    pub importance: f32,
    pub relevance: f32,
    pub created_tick: u64,
    pub created_turn: u64,
    pub last_access_turn: u64,
    /// Last user turn the item was selected into a materialized model
    /// surface (see `ContextItem::last_selected_turn`).
    #[serde(default)]
    pub last_selected_turn: u64,
    pub access_count: u32,
    /// Ids of prior items this item explicitly depends on (shared entities).
    #[serde(default)]
    pub dependencies: Vec<ContextItemId>,
    /// Model/operator-directed GC hint (see `ContextItem::keep_alive`).
    #[serde(default)]
    pub keep_alive: bool,
    /// Model/operator-directed lease (see `ContextItem::lease_until_turn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until_turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[async_trait]
pub trait ContextEngine: Send + Sync {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()>;

    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport>;

    /// Run a full GC pass: mark roots (pins, active scopes, hot entities,
    /// reachable dependencies), sweep unmarked items into the bounded
    /// reversible eviction buffer and reactivate items that became relevant
    /// again. The report explains every eviction and reactivation. The
    /// default implementation does nothing, so engines without a GC pass
    /// (baselines, adapters) keep working unchanged.
    async fn gc(&self) -> AgentResult<ContextGcReport> {
        Ok(ContextGcReport::default())
    }

    /// Materialize the working set for one model request: score, budget-pack
    /// and expand the selection, returning structured items. The runtime
    /// turns them into prompt text via its prompt assembler.
    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext>;

    /// Commit the exact subset of a materialization preview that a successful
    /// model operation consumed. Engines without reinforcement state may keep
    /// the validated default no-op; stateful engines must reject stale ids or
    /// ids that were not part of the referenced preview.
    async fn acknowledge_consumption(&self, ack: ContextConsumptionAck) -> AgentResult<()> {
        ack.validate()
    }

    /// Open a fresh scope under `parent` (or the current active scope when
    /// `parent` is `None`) and make it the active scope. The runtime drives
    /// tool scopes this way — a scope opens when its tool starts, not when
    /// the observation is later persisted. Items created while the scope is
    /// active carry its id.
    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId>;

    /// Close a scope: mark it closed, promote its durable members to the
    /// nearest open ancestor, reactivate the parent, and record the
    /// transitions the close produced.
    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>>;

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics>;

    /// Where a workspace path currently lives, for `fs.read` attribution.
    /// Read-only. Default treats the path as a first read so engines without
    /// a catalog cannot pretend they measured GC-caused rereads.
    async fn fs_read_residency(&self, path: &str) -> AgentResult<FsRereadClass> {
        let _ = path;
        Ok(FsRereadClass::FirstRead)
    }

    /// Run the conservative Storage GC: permanently delete context-store
    /// entries whose semantic lifecycle ended and that nothing references
    /// anymore. This is the *only* place information is deleted — Context
    /// GC externalizes, it never purges. Default implementation does
    /// nothing, so engines without a store (baselines, adapters) keep
    /// working unchanged.
    async fn storage_gc(&self) -> AgentResult<StorageGcReport> {
        Ok(StorageGcReport::default())
    }

    /// Bring the store back in line with the external map after a crash or
    /// an interrupted IO phase: every formal blob gets exactly one owner
    /// (rebuilt entry or deleted as a stale duplicate of resident content),
    /// corrupt/id-mismatched blobs are quarantined, and abandoned temp
    /// files are removed. The composition root calls this at startup after
    /// restoring the checkpoint. Default implementation does nothing, so
    /// engines without a store keep working unchanged.
    async fn reconcile_store(&self) -> AgentResult<StoreReconcileReport> {
        Ok(StoreReconcileReport::default())
    }

    /// Bounded projection of live items, oldest first, capped at `limit`.
    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>>;

    /// Deterministic catalog search: matches the query against entity
    /// signatures, kind/scope/task/label filters and recency, capped at
    /// `query.limit`. Hits may be Resident, Warm, Cold, or External so a
    /// live catalog file is not an empty miss. Catalog residency is not the
    /// selected working set. The
    /// default implementation returns nothing, so engines without a catalog
    /// (baselines) keep working unchanged.
    async fn search_external(
        &self,
        query: ContextSearchQuery,
    ) -> AgentResult<Vec<ExternalizedContext>> {
        let _ = query;
        Ok(Vec::new())
    }

    /// One catalog entry's metadata by item id. Resident/Warm projections
    /// need no store read; stored entries use the map descriptor. Default
    /// returns nothing.
    async fn inspect_external(
        &self,
        item_id: ContextItemId,
    ) -> AgentResult<Option<ExternalizedContext>> {
        let _ = item_id;
        Ok(None)
    }

    /// Pull one catalog item's full content. Resident/Warm bodies come from
    /// the in-memory catalog; Cold/External bodies come from the store. The
    /// item stays at its current residency — this is a stamped read, not a
    /// reactivation. Catalog residency is not the selected working set.
    async fn fetch_external(&self, item_id: ContextItemId) -> AgentResult<Option<ContextItem>> {
        let _ = item_id;
        Ok(None)
    }

    /// Export the current runtime state (separate from the event journal).
    async fn checkpoint(&self) -> AgentResult<serde_json::Value>;

    /// Replace runtime state from a previously exported checkpoint.
    async fn restore(&self, data: serde_json::Value) -> AgentResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_root_strengths_are_independent_protections() {
        assert!(AnchorRootStrength::PromptRequired.requires_prompt());
        assert!(AnchorRootStrength::PromptRequired.requires_residency());
        assert!(!AnchorRootStrength::PromptRequired.requires_storage());

        assert!(!AnchorRootStrength::ResidentRequired.requires_prompt());
        assert!(AnchorRootStrength::ResidentRequired.requires_residency());
        assert!(!AnchorRootStrength::ResidentRequired.requires_storage());

        assert!(!AnchorRootStrength::StorageRequired.requires_prompt());
        assert!(!AnchorRootStrength::StorageRequired.requires_residency());
        assert!(AnchorRootStrength::StorageRequired.requires_storage());

        assert!(!AnchorRootStrength::Recallable.requires_prompt());
        assert!(!AnchorRootStrength::Recallable.requires_residency());
        assert!(!AnchorRootStrength::Recallable.requires_storage());
    }

    fn ref_entry() -> ExternalizedContext {
        ExternalizedContext {
            item_id: ContextItemId::new(),
            task_id: None,
            scope_id: None,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            retention: ContextRetention::Working,
            attention: AttentionState::Archived,
            semantic: SemanticState::Live,
            context_ref: ContextRef {
                uri: "context://run/x".into(),
                item_id: ContextItemId::new(),
                kind: ContextKind::Note,
                scope: ContextScope::Task,
                summary: "x".into(),
                created_tick: 0,
            },
            externalized_at_tick: 0,
            last_access_tick: 0,
            residency: ContextResidency::Cold,
            entities: Vec::new(),
            tags: Vec::new(),
            dependencies: Vec::new(),
            keep_alive: false,
            lease_until_turn: None,
            last_access_gc_epoch: Some(0),
            blob_checksum: None,
            source: None,
            importance: 0.0,
            relevance: 0.0,
            created_tick: 0,
            created_turn: 0,
            last_access_turn: 0,
            last_selected_turn: 0,
            access_count: 0,
            last_access_signal: AccessSignal::None,
            search_reinforce_count: 0,
            gc_generation: 0,
            evicted_at_tick: None,
            file_path: None,
            file_revision: None,
        }
    }

    #[test]
    fn externalized_entry_without_the_new_authority_fields_still_loads() {
        // 权威元数据字段全部带 serde(default)：旧 checkpoint / 旧 wire
        // 数据（没有 importance/created_tick/... ）必须能反序列化为默认值，
        // 而不是拒绝加载。
        let legacy = serde_json::json!({
            "item_id": ContextItemId::new(),
            "kind": "Note",
            "scope": "Task",
            "retention": "Working",
            "attention": "Archived",
            "semantic": "Live",
            "context_ref": {
                "uri": "context://run/x",
                "item_id": ContextItemId::new(),
                "kind": "Note",
                "scope": "Task",
                "summary": "legacy",
                "created_tick": 0
            },
            "externalized_at_tick": 0,
            "last_access_tick": 0,
            "residency": "Cold",
            "entities": [],
            "tags": [],
            "dependencies": [],
            "keep_alive": false,
            "lease_until_turn": null,
            "last_access_gc_epoch": 0,
            "blob_checksum": null
        });
        let entry: ExternalizedContext = serde_json::from_value(legacy).unwrap();
        assert_eq!(entry.importance, 0.0);
        assert_eq!(entry.relevance, 0.0);
        assert_eq!(entry.created_tick, 0);
        assert_eq!(entry.created_turn, 0);
        assert_eq!(entry.last_access_turn, 0);
        assert_eq!(entry.last_selected_turn, 0);
        assert_eq!(entry.access_count, 0);
        assert_eq!(entry.last_access_signal, AccessSignal::None);
        assert_eq!(entry.search_reinforce_count, 0);
        assert_eq!(entry.gc_generation, 0);
        assert_eq!(entry.evicted_at_tick, None);
        assert_eq!(entry.source, None);
    }

    #[test]
    fn context_map_view_caps_at_the_contract_bound() {
        let entries: Vec<ExternalizedContext> =
            (0..CONTEXT_MAP_VIEW_CAP).map(|_| ref_entry()).collect();
        let view = ContextMapView::new(entries);
        assert_eq!(view.len(), CONTEXT_MAP_VIEW_CAP);
        assert_eq!(view.iter().count(), CONTEXT_MAP_VIEW_CAP);
        assert_eq!(view.as_slice().len(), CONTEXT_MAP_VIEW_CAP);
    }

    #[test]
    #[should_panic(expected = "exceeds the cap")]
    fn context_map_view_rejects_an_over_cap_build() {
        let entries: Vec<ExternalizedContext> =
            (0..CONTEXT_MAP_VIEW_CAP + 1).map(|_| ref_entry()).collect();
        let _ = ContextMapView::new(entries);
    }

    #[test]
    fn context_map_view_serializes_transparently_and_validates_wire_data() {
        let mut entries: Vec<ExternalizedContext> =
            (0..CONTEXT_MAP_VIEW_CAP).map(|_| ref_entry()).collect();
        entries.push(ref_entry());
        // Over-cap wire data is rejected by deserialization (the bound is
        // enforced on both sides of the service boundary), while the local
        // constructor's invariant keeps the in-process shape honest.
        let bytes = serde_json::to_vec(&entries).unwrap();
        let err = serde_json::from_slice::<ContextMapView>(&bytes).unwrap_err();
        assert!(err.to_string().contains("exceeds the cap"));
    }

    #[test]
    fn context_consumption_ack_enforces_bounded_unique_id_sets() {
        let valid = ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 0,
            materialization_id: 1,
            item_ids: vec![ContextItemId::new()],
            external_item_ids: vec![ContextItemId::new()],
            foreground_item_ids: vec![ContextItemId::new()],
        };
        valid.validate().unwrap();

        let duplicate = ContextItemId::new();
        let mut invalid = valid.clone();
        invalid.item_ids = vec![duplicate, duplicate];
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate item ids")
        );

        invalid.item_ids = (0..=CONTEXT_CONSUMPTION_ACK_ITEM_CAP)
            .map(|_| ContextItemId::new())
            .collect();
        assert!(invalid.validate().unwrap_err().to_string().contains("cap"));

        invalid.item_ids = vec![duplicate];
        invalid.external_item_ids = vec![duplicate];
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate item identity")
        );

        // Duplicate foreground identities are rejected; an overlap between
        // a foreground body and a selected item is not (the same body can
        // legitimately appear in both roles in one frame).
        let foreground_duplicate = ContextItemId::new();
        invalid.item_ids.clear();
        invalid.external_item_ids.clear();
        invalid.foreground_item_ids = vec![foreground_duplicate, foreground_duplicate];
        assert!(
            invalid
                .validate()
                .unwrap_err()
                .to_string()
                .contains("duplicate foreground")
        );
        invalid.foreground_item_ids = vec![duplicate];
        invalid.item_ids = vec![duplicate];
        assert!(invalid.validate().is_ok());

        invalid.item_ids.clear();
        invalid.foreground_item_ids.clear();
        invalid.external_item_ids = (0..=CONTEXT_MAP_VIEW_CAP)
            .map(|_| ContextItemId::new())
            .collect();
        assert!(invalid.validate().unwrap_err().to_string().contains("cap"));
    }

    #[test]
    fn dependency_edge_roundtrips_and_accepts_the_legacy_id_form() {
        let target = ContextItemId::new();
        let edge = DependencyEdge::shares(target);

        // The typed wire form carries the edge kind...
        let json = serde_json::to_string(&edge).unwrap();
        assert!(json.contains("shares_entities"), "{json}");
        let back: DependencyEdge = serde_json::from_str(&json).unwrap();
        assert_eq!(back, edge);

        // ...and the pre-typed form (a bare id string, as written by old
        // checkpoints) still deserializes as a SharesEntities edge.
        let legacy: DependencyEdge = serde_json::from_str(&format!("\"{target}\"")).unwrap();
        assert_eq!(legacy, DependencyEdge::shares(target));
    }

    #[test]
    fn legacy_dependency_arrays_in_checkpoints_still_load() {
        let target = ContextItemId::new();
        let legacy: Vec<DependencyEdge> = serde_json::from_str(&format!("[\"{target}\"]")).unwrap();
        assert_eq!(legacy, vec![DependencyEdge::shares(target)]);
    }

    #[test]
    fn dependency_kind_consumers_keep_affinity_citation_prompt_and_residency_orthogonal() {
        use DependencyKind::*;
        let kinds = [
            SharesEntities,
            DerivedFrom,
            EvidenceFor,
            VerifiedBy,
            ArtifactOf,
            Continuation,
        ];
        for kind in kinds {
            assert_eq!(
                kind.is_strong(),
                kind.protects_storage(),
                "{kind:?}: is_strong is the storage-citation alias"
            );
            if kind == SharesEntities {
                assert!(!kind.requires_prompt_body());
                assert!(!kind.requires_residency());
                assert!(!kind.protects_storage());
            }
        }
        assert!(SharesEntities.affects_ranking() && !SharesEntities.requires_prompt_body());
        assert!(SharesEntities.affects_ranking() && !SharesEntities.requires_residency());
        assert!(!SharesEntities.protects_storage());

        assert!(!DerivedFrom.affects_ranking());
        assert!(!DerivedFrom.requires_prompt_body());
        assert!(!DerivedFrom.requires_residency());
        assert!(DerivedFrom.protects_storage());

        assert!(EvidenceFor.affects_ranking());
        assert!(!EvidenceFor.requires_prompt_body());
        assert!(!EvidenceFor.requires_residency());
        assert!(EvidenceFor.protects_storage());

        assert!(!VerifiedBy.requires_prompt_body() && !VerifiedBy.requires_residency());
        assert!(VerifiedBy.protects_storage());
        assert!(!ArtifactOf.requires_prompt_body() && !ArtifactOf.requires_residency());
        assert!(ArtifactOf.protects_storage());

        assert!(Continuation.affects_ranking());
        assert!(Continuation.requires_prompt_body());
        assert!(Continuation.requires_residency());
        assert!(Continuation.protects_storage());
    }

    #[test]
    fn default_coding_prompt_states_the_runtime_contract() {
        let prompt = DEFAULT_CODING_AGENT_SYSTEM_PROMPT;
        assert!(prompt.contains("focused coding agent"));
        assert!(prompt.contains("not the full catalog"));
        assert!(prompt.contains("context tools"));
        assert!(prompt.contains("capability tools"));
        assert!(
            !prompt.contains("bounded cache"),
            "calling the working set a cache made the model re-read it"
        );
        assert!(
            !prompt.contains("Use context.manage") && !prompt.contains("prefer Live"),
            "the contract names surfaces, not retrieval steps: {prompt}"
        );
        let sentences = prompt
            .split('.')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .count();
        assert!(
            (3..=4).contains(&sentences),
            "keep the contract short, got {sentences} sentences: {prompt}"
        );
    }

    #[test]
    fn path_exactly_in_directive_is_a_path_token_not_a_substring() {
        assert!(path_exactly_in_directive(
            "Append to src/scratch.md: HDMI is in drawer 3",
            "src/scratch.md"
        ));
        assert!(path_exactly_in_directive(
            "Append to `src/scratch.md`",
            "src/scratch.md"
        ));
        assert!(path_exactly_in_directive(
            "edit scratch.md please",
            "src/scratch.md"
        ));
        assert!(!path_exactly_in_directive(
            "look at src/utils.py",
            "src/util.py"
        ));
        assert!(!path_exactly_in_directive(
            "look at src/util.py",
            "src/utils.py"
        ));
        assert!(!path_exactly_in_directive("open ba.rs", "a.rs"));
        assert!(!path_exactly_in_directive(
            "file.rs.bak is stale",
            "file.rs"
        ));
    }
}
