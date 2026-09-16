use agent_contracts::{
    AgentError, AgentResult, BoundedCompactor, CompactionOutput, CompactionRequest, ContextAction,
    ContextCompaction, ContextConsumptionAck, ContextDiagnostics, ContextEngine, ContextGcReport,
    ContextIngress, ContextItem, ContextItemId, ContextItemSummary, ContextKind,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextRetention,
    ContextScope, ContextSearchCoverage, ContextSearchCoverageStop, ContextSearchObservation,
    ContextStateTransition, CoreLabel, FocusState, FsRereadClass, Label, MAX_RESOURCE_TOUCHES,
    MaterializedContext, ScopeId, ScopeKind, ScopeState, StoreReconcileReport, UsageIdentity,
    bound_compaction_output, normalize_resource_path,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::checkpoint;
use crate::diagnostics;
use crate::distill::{
    DistillJob, insert_derived_summary, insert_task_summary, plan_episode_distill,
};
use crate::gc::{full, minor, reachability};
use crate::heap::external_summary;
use crate::index::{dependency, entity};
use crate::item;
use crate::materializer;
use crate::scope;
use crate::store;

/// Bounded row budget for explainable report vectors (maintenance
/// transitions, GC evictions, storage-GC and reconcile reasons). Passes may
/// *count* far more rows; only the report rows are capped, with the omitted
/// count surfaced in the report.
pub(crate) const MAX_REPORT_ROWS: usize = 4096;

/// Cap a report vector to the bounded row budget, returning how many rows
/// were omitted. The caller keeps its own authoritative counters; this only
/// bounds the explainable rows.
pub(crate) fn truncate_report_rows<T>(rows: &mut Vec<T>, cap: usize) -> usize {
    if rows.len() <= cap {
        return 0;
    }
    let omitted = rows.len() - cap;
    rows.truncate(cap);
    omitted
}

/// 一条 anchor 根声明的 item_ref 是否匹配给定 id + entity 签名：精确
/// id、`context://run/<id>` uri，或精确 entity 名。TaskAnchor 声明的是
/// 引用（不嵌入 body），engine 只按这三类键解析它指向谁。
pub(crate) fn anchor_ref_matches(item_ref: &str, id: ContextItemId, entities: &[String]) -> bool {
    item_ref == id.to_string()
        || item_ref == format!("context://run/{id}")
        || entities.iter().any(|entity| entity == item_ref)
}

/// 一条 anchor 根声明是否匹配某个 resident 条目（mark 与 materialize
/// 共用同一解析，保证 GC 根与强制入帧看到同一目标集）。
pub(crate) fn anchor_claim_matches_item(
    claim: &agent_contracts::AnchorRootClaim,
    item: &ContextItem,
) -> bool {
    anchor_ref_matches(&claim.item_ref, item.id, &item.entities)
}

/// 一条 anchor 根声明是否匹配某个外部映射条目（Cold/External 的召回与
/// Storage GC 保护共用同一解析）。
pub(crate) fn anchor_claim_matches_entry(
    claim: &agent_contracts::AnchorRootClaim,
    entry: &agent_contracts::ExternalizedContext,
) -> bool {
    anchor_ref_matches(&claim.item_ref, entry.item_id, &entry.entities)
}

/// CTX-5（R2-03）：唯一的 live 保护判定——当前投影里一条
/// ResidentRequired/PromptRequired 声明命中的 *live* 条目，被一切启发式
/// 终结路径（驻留 TTL/ttl×4、warm 老化、full sweep 的普通对话老化）豁免，
/// Resident/Warm/Stored 四个正文位置一致。`state.anchor_roots` 是运行时
/// 在每次 GC/materialize 前整集替换的当前投影（释放＝空投影），所以旧/空
/// 投影不会冒充当前根；anchor_revision 随声明保留在报告里。调用方只在
/// semantic liveness 之后评估本判定，Superseded/VerifiedFixed/Tombstoned
/// 保持终态——保护绝不复活。StorageRequired 只保护储存，从不延长驻留。
pub(crate) fn anchor_claim_defers_expiry(state: &State, item: &ContextItem) -> bool {
    item.semantic.is_live()
        && state.anchor_roots.iter().any(|claim| {
            claim.strength.requires_residency() && anchor_claim_matches_item(claim, item)
        })
}

#[derive(Debug, Clone)]
pub struct SimpleContextConfig {
    /// Frozen Context V1 operational knob. Do not retune to chase extra
    /// rounds; auto-reactivation is no longer the live extra-round driver.
    pub active_threshold: f32,
    /// Frozen Context V1 operational knob. Do not retune to chase extra rounds.
    pub archive_threshold: f32,
    pub turn_ttl_ticks: u64,
    pub max_item_chars: usize,
    /// Detect superseding decisions and archive the superseded ones.
    pub supersession: bool,
    /// Error -> fix -> verified lifecycle (errors persist until a
    /// successful host probe in the same task verifies the fix).
    pub error_verification: bool,
    /// Reward items whose entity signature is hot (last user message +
    /// recent tool observations).
    pub entity_affinity: bool,
    /// Record explicit dependency edges between items sharing entities
    /// (affinity) and expand the working set with Continuation bodies.
    pub dependency_expansion: bool,
    /// Run the full GC pass (mark roots, sweep, reversible eviction) when
    /// `ContextEngine::gc` is invoked.
    pub gc_enabled: bool,
    /// Items surviving this many full passes without root reachability are
    /// eviction candidates (the generational dimension of GC). Frozen
    /// Context V1 operational knob. Do not retune to chase extra rounds.
    pub gc_max_generation: u32,
    /// Cap on the reversible eviction buffer; overflow no longer purges —
    /// items are externalized to the context store instead.
    pub gc_buffer_capacity: usize,
    /// Max items reactivated per GC pass (newest first).
    pub gc_reactivate_per_pass: usize,
    /// Directory of the external context store: eviction-buffer overflow
    /// writes full items here and keeps only a lightweight `ContextRef`
    /// entry. The composition root injects `workspace.state_dir()/context-store`
    /// so runtime state never scatters under a CWD; `None` falls back to an
    /// OS temp dir scoped to this process (never a CWD-relative path).
    pub context_store_dir: Option<std::path::PathBuf>,
    /// Full GC passes an externalized (`Cold`) entry may sit in memory
    /// before it ages to `External` (only the store retains it). The unit
    /// is *generations*: only a full GC increments `State::gc_epoch`, so
    /// the TTL counts real passes, not the tick counter (which also grows
    /// on ingest/maintain/materialize).
    pub gc_external_ttl_generations: u32,
    /// Storage GC only deletes store entries whose semantic lifecycle ended
    /// at least this many ticks ago and that nothing references.
    pub storage_ttl_ticks: u64,
    /// Cap on how many items may carry `keep_alive` at once. Model hints are
    /// hints: a runaway `gc_hint keep=true` must not root the whole heap.
    pub max_keep_alive_items: usize,
    /// Cap on lease turns per directive. A lease is bounded, not permanent —
    /// the model cannot lease an item "forever" with one call.
    pub max_lease_turns: u32,
    /// Cap on leased items per task (count). A task cannot lease its whole
    /// history into roots.
    pub max_leased_items_per_task: usize,
    /// Cap on total content tokens leased per task. Count + tokens together
    /// bound both the number and the weight of model-protected items.
    pub max_leased_tokens_per_task: usize,
    /// Cap on `context.admit` calls per turn: admit re-enters items into
    /// the working set, so a runaway admit loop must not grow the heap
    /// without bound between GC passes.
    pub max_admits_per_turn: usize,
    /// Cap on `context.derive` calls per turn: each derive persists a new
    /// observation, so the model cannot mint derived items without bound.
    pub max_derived_items_per_turn: usize,
    /// Minimum token overlap between a new user instruction and the current
    /// episode's query for the instruction to count as a continuation of the
    /// same episode. Below this, and when the message carries real
    /// information (entities or length), the focus episode rotates: durable
    /// outcomes promote to the task scope, ordinary dialogue is evicted.
    pub episode_rotate_threshold: f32,
    /// Hard cap on user turns per focus episode. Even when every message is
    /// a semantic continuation, the episode rotates at this budget so a
    /// pathological single-episode run cannot grow the working set without
    /// bound.
    pub episode_max_user_turns: usize,
    /// Cap on the in-engine lifecycle ledger buffer. The ledger is bounded
    /// (oldest rows drop) and is exported to a JSONL artifact on demand —
    /// never written on the context hot path.
    pub max_ledger_records: usize,
    /// Items (or store entries) one minor/aging pass will scan. Heaps at
    /// or below this batch still run a full stable-order pass. Larger
    /// stores resume from `GcWorkCursor` on the next event.
    pub gc_work_batch: usize,
    /// Ablation: pay for an LLM episode card even without a semantic delta.
    /// Default false. Do not retune production policy from this flag.
    pub force_episode_llm_distill: bool,
    /// User-hot entities last this many *additional* user turns after a
    /// structured resource touch. Default 2. Tool stdout never seeds this.
    pub tool_hot_ttl_turns: u64,
    /// Cap on "latest body of each recent file" residency roots. Default 8.
    /// Ablation compares 8 vs 1.
    pub recent_file_bodies: usize,
    /// Extra expiry for those file-body roots, in user turns. `0` keeps
    /// them until they fall out of the cap; `1` is a one-round lease.
    pub recent_file_body_lease_turns: u64,
    /// Ablation: ToolObservation hot matches stay Warm/Stored (descriptor
    /// reachable). Decision / Constraint / Error / OpenLoop still
    /// auto-reactivate bodies. Default false (current policy).
    pub descriptor_only_tool_observation_reactivation: bool,
    /// CTX-8 (R2-06): hard cap on the externalize-retry list. When the list
    /// holds this many owners, a pass defers further buffer overflow (the
    /// items stay owned in the buffer) and reports
    /// `externalize_backpressure` — the runtime consumes that flag to stop
    /// feeding new input instead of letting memory grow without bound.
    /// Default 4096.
    pub max_pending_externalize_items: usize,
    /// CTX-8: how many retry-list owners one full GC pass serializes and
    /// attempts to write (oldest first). Bounds the per-pass serialization
    /// bytes and store IO independent of the backlog size; the rest stay
    /// owned and retry on later passes. Default 64.
    pub gc_externalize_batch: usize,
    /// CTX-9 (R2-07): full GC retires closed, fully unreferenced scope
    /// nodes down to this target size, keeping a bounded fact note per
    /// retirement (completion facts survive via `task_completed`). The
    /// tree, memory use and checkpoint size stop growing with the number
    /// of finished tool frames and tasks. Default 1024.
    pub scope_retire_target: usize,
    /// CTX-9 残余：checkpoint 外置尾分片的内联目标——最旧的超额
    /// External 条目卡片进 store，checkpoint 只携带内联段＋寻址清单。
    /// 默认远超正常工作集（冻结策略未动，opt-in 收紧）。
    pub external_checkpoint_inline_target: usize,
    /// 单次 capture 的卡片写入预算（读校验不计入）：预算耗尽时剩余条目
    /// 本次保持内联（checkpoint 如实变大），下次 capture 继续收敛。
    pub external_checkpoint_card_batch: usize,
    /// F2: how many *not yet carded* entries one capture may serialize while
    /// planning the spill. This is the lock-held work budget: entries whose
    /// card is already on disk cost one hash lookup and are free, so a
    /// capture pays for changed entries only, and anything past the window
    /// stays inline until a later capture.
    pub external_checkpoint_scan_budget: usize,
    /// F2: total card bytes one capture may serialize (the peak the plan
    /// holds in memory) and therefore write. Entries whose card is already
    /// on disk cost nothing here — they enter the manifest from the
    /// recorded-card directory — so this bounds the changed-entry work.
    pub external_checkpoint_card_bytes: u64,
    /// F2: wall-clock budget for one capture's off-lock card I/O batch.
    /// When it is spent, the remaining planned cards stay inline and the
    /// next capture continues; a slow disk cannot stretch one capture
    /// without bound.
    pub external_checkpoint_io_budget_ms: u64,
    /// F2: how many spill cards one restore reads before it returns. The
    /// rest stay a pending directory of `(id, card hash)` rows that page in
    /// on demand, so restore cost does not track total history length.
    pub external_restore_card_batch: usize,
    /// T4: how many pending spill cards ONE operation (search, full GC,
    /// reconcile, storage GC, directive ingest) may page in before it must
    /// report a typed incomplete result instead of silently paying a total
    /// that tracks history length. The pending queue is never dropped: the
    /// next operation continues where this one stopped, and a direct id
    /// lookup (`fetch`/`inspect`/a directive's own target) pages its card
    /// regardless of this budget. Default 4096 — generous enough that normal
    /// workloads still drain in one pass, tight enough that a pathological
    /// cold tail cannot stretch one operation without bound.
    pub external_hydrate_max_items: usize,
    /// T4: wall-clock budget for one operation's hydration drain, measured
    /// from the operation's start. Spent, it stops the drain between read
    /// batches and the typed outcome reports the unread remainder — a slow
    /// disk cannot turn one search into an unbounded wait. Default 2000 ms
    /// (the same order as the capture-side IO budget).
    pub external_hydrate_budget_ms: u64,
    /// T4: hard cap on resident external metadata entries (the hot
    /// directory). Once the map holds this many entries, bulk hydration
    /// stops paging more in — the typed outcome says so, and per-id service
    /// (a fetch/inspect/directive naming a pending row) keeps working. No
    /// entry is ever dropped to enforce this: it bounds hot residency, never
    /// owners, and Storage GC's own B2 deferral covers the unread edges.
    /// Default 8192 (well above the inline target and restore batch, so it
    /// only binds on collections deliberately larger than the hot budget).
    pub external_hot_metadata_max_entries: usize,
    /// T4: soft byte cap on resident external metadata, checked between
    /// hydration batches against a cheap per-entry estimate (uri + summary +
    /// entities + a fixed overhead — not a serialized size). Like the entry
    /// cap it stops bulk paging only; owners are never dropped. Default
    /// 32 MiB.
    pub external_hot_metadata_max_bytes: u64,
    /// W2 (V4): chain-level cap on how many item ids ONE continuation walk
    /// may accumulate in its covered set before the chain closes with an
    /// explicit stop and the state is released. A fresh (no-token) search
    /// never inherits the set, so only a resumed walk can approach it; the
    /// caller restarts with a fresh search. `ContextItemId` is a fixed
    /// 16-byte value, so the entry count is also the byte bound
    /// (16,384 ids ≈ 256 KiB). Default 16,384.
    pub search_continuation_max_covered_ids: usize,
}

impl Default for SimpleContextConfig {
    fn default() -> Self {
        // Freeze-pinned 2026-08-21 (Execution Coherence item 21). Auto-
        // reactivation left the extra-round problem; do not retune these
        // three knobs or the reactivation scorer to chase live rounds.
        Self {
            active_threshold: 0.58,
            archive_threshold: 0.24,
            turn_ttl_ticks: 5,
            max_item_chars: 16_000,
            supersession: true,
            error_verification: true,
            entity_affinity: true,
            dependency_expansion: true,
            gc_enabled: true,
            gc_max_generation: 3,
            gc_buffer_capacity: 256,
            gc_reactivate_per_pass: 8,
            context_store_dir: None,
            gc_external_ttl_generations: 4,
            storage_ttl_ticks: 40,
            max_keep_alive_items: 16,
            max_lease_turns: 32,
            max_leased_items_per_task: 16,
            max_leased_tokens_per_task: 4096,
            max_admits_per_turn: 8,
            max_derived_items_per_turn: 8,
            episode_rotate_threshold: 0.15,
            episode_max_user_turns: 500,
            max_ledger_records: 4096,
            gc_work_batch: 4096,
            force_episode_llm_distill: false,
            tool_hot_ttl_turns: 2,
            recent_file_bodies: crate::index::entity::MAX_RECENT_FILE_BODIES,
            recent_file_body_lease_turns: 0,
            descriptor_only_tool_observation_reactivation: false,
            max_pending_externalize_items: 4096,
            gc_externalize_batch: 64,
            scope_retire_target: 1024,
            external_checkpoint_inline_target: 2048,
            external_checkpoint_card_batch: 64,
            external_checkpoint_scan_budget: 4096,
            external_checkpoint_card_bytes: 8 * 1024 * 1024,
            external_checkpoint_io_budget_ms: 2_000,
            external_restore_card_batch: 256,
            external_hydrate_max_items: 4096,
            external_hydrate_budget_ms: 2_000,
            external_hot_metadata_max_entries: 8192,
            external_hot_metadata_max_bytes: 32 * 1024 * 1024,
            search_continuation_max_covered_ids: 16_384,
        }
    }
}

impl SimpleContextConfig {
    /// The baseline policy: no supersession, no error verification, no entity
    /// affinity, no dependency graph. Kept for A/B/C comparison so the policy
    /// delta is measurable.
    pub fn baseline_v0() -> Self {
        Self {
            supersession: false,
            error_verification: false,
            entity_affinity: false,
            dependency_expansion: false,
            ..Self::default()
        }
    }
}

/// T4: why one operation's pending-card drain stopped short. The typed
/// difference matters to callers: `Budget`, `Deadline` and `HotCap` are
/// resumable states (the queue is untouched, the next operation continues),
/// `Unreadable` is the B2 transient-I/O state — all four mean "the in-memory
/// external set is incomplete", never "there is nothing left".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HydrationStop {
    /// Every pending row was consumed: the external set in memory is
    /// complete.
    Complete,
    /// The operation's items budget or wall-clock deadline was spent between
    /// read batches.
    Budget,
    /// S3: a single cold read was cancelled at the deadline boundary; its
    /// row keeps its pending owner and the drain returns instead of waiting
    /// out the budget on one slow read.
    Deadline,
    /// The resident-metadata cap (entries or estimated bytes) is reached;
    /// further cold metadata is served per id, not bulk-paged.
    HotCap,
    /// Every remaining row hit a transient read failure this pass (the
    /// pre-T4 B2 shape).
    Unreadable,
}

/// T4: typed result of one operation's bounded hydration drain. Replaces the
/// bare `bool` completeness flag: callers see how much is left and why the
/// drain stopped, and the pending queue always stays resumable (N02 rows
/// keep their owner; nothing is consumed but a verified outcome).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HydrationOutcome {
    /// True only when the pending queue drained fully this pass.
    pub(crate) complete: bool,
    /// Pending rows left unread — the queue keeps them, and this count is
    /// what typed consumers (storage-GC deferral, search fail-closed)
    /// surface.
    pub(crate) remaining: usize,
    pub(crate) stopped: HydrationStop,
}

impl HydrationOutcome {
    pub(crate) fn complete() -> Self {
        Self {
            complete: true,
            remaining: 0,
            stopped: HydrationStop::Complete,
        }
    }

    fn stopped(stopped: HydrationStop, remaining: usize) -> Self {
        Self {
            complete: false,
            remaining,
            stopped,
        }
    }
}

/// T4: one operation's hydration budget — items plus an absolute deadline
/// plus the resident-metadata caps — built from the config at the
/// operation's start so every caller shares one definition of "this
/// operation's fair share of cold reads".
pub(crate) struct HydrationBudget {
    /// Pending cards this operation may page in.
    pub(crate) max_items: usize,
    /// Absolute wall-clock deadline for the whole drain.
    pub(crate) deadline: std::time::Instant,
    /// Resident external-metadata entry cap (bulk paging stops at it).
    pub(crate) hot_max_entries: usize,
    /// Resident external-metadata byte cap (estimated; checked between
    /// batches).
    pub(crate) hot_max_bytes: u64,
}

impl HydrationBudget {
    pub(crate) fn for_operation(config: &SimpleContextConfig) -> Self {
        Self {
            max_items: config.external_hydrate_max_items,
            deadline: std::time::Instant::now()
                + std::time::Duration::from_millis(config.external_hydrate_budget_ms),
            hot_max_entries: config.external_hot_metadata_max_entries,
            hot_max_bytes: config.external_hot_metadata_max_bytes,
        }
    }

    /// Deterministic budget for tests that exercise one batch's settlement
    /// arithmetic, not the budget clock.
    #[cfg(test)]
    pub(crate) fn for_tests() -> Self {
        Self {
            max_items: usize::MAX,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            hot_max_entries: usize::MAX,
            hot_max_bytes: u64::MAX,
        }
    }
}

/// S3: measured resident-metadata residency against the caps. One
/// measurement implementation shared by the bulk drain's stop check and the
/// settlement entry — the estimate is
/// `entry_metadata_bytes_estimate` summed over the hot map (fixed 256
/// bytes/entry + 64 bytes/dependency + uri/summary/entity lengths). It
/// bounds the estimated metadata footprint; it is not RSS and never decides
/// ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MetadataResidencyPressure {
    pub(crate) entries: usize,
    pub(crate) bytes_estimate: u64,
    pub(crate) over_entries: usize,
    pub(crate) over_bytes: u64,
}

impl MetadataResidencyPressure {
    pub(crate) fn measure(
        external: &crate::index::external::ExternalMap,
        max_entries: usize,
        max_bytes: u64,
    ) -> Self {
        let entries = external.len();
        let bytes_estimate = external.metadata_bytes_estimate();
        Self {
            entries,
            bytes_estimate,
            over_entries: entries.saturating_sub(max_entries),
            over_bytes: bytes_estimate.saturating_sub(max_bytes),
        }
    }

    pub(crate) fn within_budget(&self) -> bool {
        self.over_entries == 0 && self.over_bytes == 0
    }
}

/// S3: the single metadata-residency settlement entry. Every install site
/// (per-id fetch/inspect/directive target, GC growth) runs its post-install
/// demotion and budget settlement through here: measure against the caps,
/// return the oldest carded, non-pinned entries that fit to the pending
/// directory (rows appended at the back, claims kept), and report the
/// residual. `protect` names ids this settlement must not demote (the entry
/// just served to the model); protected entries still count toward the
/// residual, so an over-budget state that cannot be repaired by demotion
/// surfaces as `over_entries`/`over_bytes` — the caller's typed
/// backpressure fact — instead of silently passing.
pub(crate) fn settle_metadata_residency(
    state: &mut State,
    config: &SimpleContextConfig,
    protect: &[ContextItemId],
) -> MetadataResidencyPressure {
    let outcome = state.external.demote_overflow(
        config.external_hot_metadata_max_entries,
        config.external_hot_metadata_max_bytes,
        protect,
    );
    if !outcome.demoted.is_empty() {
        state.pending_external_cards.extend(outcome.demoted);
        state.sync_catalog();
    }
    MetadataResidencyPressure::measure(
        &state.external,
        config.external_hot_metadata_max_entries,
        config.external_hot_metadata_max_bytes,
    )
}

/// S3: outcome of one bounded batch of pending-card reads. Every counter is
/// a distinct settlement fact: `installed` entries entered the hot map;
/// `consumed_missing` rows were verified absent/damaged and left the queue;
/// `io_failed` rows hit a transient read error and stay queued;
/// `timed_out` rows were cancelled at the deadline boundary and stay
/// queued; `oversized` rows fit no cap room (entry count or estimated
/// metadata bytes) and stay queued as addressable cold owners; `rotated`
/// rows were moved from the front to the back of the queue so later
/// readable pages are not permanently blocked by an unreadable front row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PendingCardBatch {
    pub(crate) installed: usize,
    pub(crate) consumed_missing: usize,
    pub(crate) io_failed: usize,
    pub(crate) timed_out: usize,
    pub(crate) oversized: usize,
    pub(crate) rotated: usize,
}

/// W1 (V2): the typed outcome of one per-id pending-card service
/// ([`SimpleContextEngine::hydrate_card_for_outcome`]). `Installed` and
/// `AlreadyOwned` mean the body was successfully served this operation;
/// `Missing` (verified absent or structurally invalid — row consumed),
/// `Corrupt` (damaged card — row consumed) and `IoFailed` (transient — row
/// stays retryable) name *why* it was not; `NoPendingRow` means the id has
/// no cold owner, so loaded-index absence is authoritative for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingIdOutcome {
    Installed,
    AlreadyOwned,
    Missing,
    Corrupt,
    IoFailed,
    NoPendingRow,
}

/// B1: one per-id pending-card service result — the typed outcome plus the
/// card's entry when the read decided in the entry's favor. The entry is the
/// version the pending row authorizes (identity, checksum and owner
/// metadata bound to that card hash): exactly what any later re-hydration of
/// the row would install. The required-ref resolution captures it so
/// planning never depends on the entry still being hot when the whole
/// resolution batch ends — a later install in the same batch may have
/// demoted an earlier target back to pending.
#[derive(Debug)]
pub(crate) struct PendingIdRead {
    pub(crate) outcome: PendingIdOutcome,
    pub(crate) entry: Option<Box<agent_contracts::ExternalizedContext>>,
}

impl PendingIdRead {
    fn without_entry(outcome: PendingIdOutcome) -> Self {
        Self {
            outcome,
            entry: None,
        }
    }
}

/// Mutable runtime state of the engine, kept behind a lock. The heap (with
/// its secondary indexes bound to it), the focus, the hot-entity set and
/// the pending lifecycle intents all live here; modules read and mutate it
/// through `pub(crate)` access.
#[derive(Debug, Default)]
pub(crate) struct PendingMaterialization {
    pub(crate) id: u64,
    item_ids: std::collections::HashSet<ContextItemId>,
    external_item_ids: std::collections::HashSet<ContextItemId>,
    pub(crate) foreground_item_ids: std::collections::HashSet<ContextItemId>,
    /// Normalized paths of the foreground bodies, with their ids, so the
    /// ack can stamp final-frame exposure for bodies the model consumed
    /// without changing their residency.
    pub(crate) foreground_paths: Vec<(ContextItemId, String)>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// Monotonic event-sequence clock: advances on every state-changing
    /// operation (ingest/maintain/GC/reconcile/ack/scope ops). Never
    /// advances on `materialize` — a preview is a read and must not age
    /// TTLs or recency. `alias = "tick"` keeps pre-separation checkpoints
    /// loadable. TTL rules name their clock explicitly; this one orders
    /// events and measures event-distance, not age.
    #[serde(default, alias = "tick")]
    pub(crate) event_seq: u64,
    /// 上一次维护完成时的序列值：与 `event_seq` 相等说明状态无变化，
    /// 此类维护触发可直接跳过——不扫描，也不消耗序列。
    #[serde(default)]
    pub(crate) last_maintained_seq: u64,
    /// User-turn clock: advances once per user message. Rules measuring
    /// age in user turns (ephemeral TTL, staleness) read this.
    pub(crate) turn: u64,
    pub(crate) tool_round: u64,
    /// Monotonic identity of the last materialization preview. Persisted so
    /// checkpoint/restore cannot reuse an id inside one engine lifetime.
    #[serde(default)]
    pub(crate) materialization_revision: u64,
    /// Single actor-owned preview awaiting a successful model-consumption
    /// acknowledgement. Ephemeral: checkpoints are taken only at safe points
    /// and must never resume an in-flight provider request.
    #[serde(skip)]
    pub(crate) pending_materialization: Option<PendingMaterialization>,
    pub(crate) focus: Option<FocusState>,
    /// The heap owns its slot/entity/scope indexes: structural mutations
    /// go through `ContextHeap` methods so the indexes cannot drift.
    pub(crate) items: crate::index::heap::ContextHeap,
    /// (item_id, by_id, reason) queued by ingest for superseded decisions,
    /// drained by maintenance so the resulting semantic state change is
    /// recorded as a lifecycle transition.
    #[serde(default)]
    pub(crate) pending_supersessions: Vec<(ContextItemId, ContextItemId, String)>,
    /// (item_id, by_id, reason) queued by ingest for verified-fixed errors.
    #[serde(default)]
    pub(crate) pending_verifications: Vec<(ContextItemId, ContextItemId, String)>,
    /// CTX-9 残余：最近一次 restore 中缺失/损坏的外置元数据卡片数。
    /// 非零＝恢复的 external 集合不完整（旧 checkpoint 的卡片已被
    /// Storage GC 清理），消费端不得把它读成完整状态。
    #[serde(default)]
    pub(crate) external_cards_missing: u64,
    /// N02：卡片读取的瞬态 I/O 失败数。与缺失分开计数——瞬时故障的
    /// locator 仍留在 pending 队列里可重试，不是「数据不存在」。
    #[serde(default)]
    pub(crate) external_card_io_failures: u64,
    /// F2: spill-card rows a restore accepted but has not paged in yet —
    /// `(item id, card hash)`. The ids are known (so blob/card deletion is
    /// deferred and an id lookup pages its card in), the metadata is not in
    /// memory. Never serialized as state: a capture re-emits these rows
    /// into the checkpoint's spill manifest, which is the same directory.
    #[serde(skip)]
    pub(crate) pending_external_cards: Vec<(ContextItemId, String)>,
    /// CTX-9: bounded fact notes for retired (fully unreferenced, closed)
    /// scopes. The nodes leave the tree and every checkpoint; the facts
    /// (id/kind/task/completion) stay addressable for `task_completed` and
    /// audit within the explicit retention ring.
    #[serde(default)]
    pub(crate) retired_scopes: Vec<crate::scope::RetiredScopeNote>,
    /// CTX-10 (R3-04): set once the bounded retirement ring has dropped a
    /// fact. From then on the ring is a *recent window*, not a complete
    /// record: a task with no live scope and no note is unknown, and the
    /// GC treats unknown as conservatively completed.
    #[serde(default)]
    pub(crate) retirement_ring_overflowed: bool,
    /// Lifecycle transitions already applied by ingest (focus episode
    /// rotation). They are surfaced by the next maintenance report so the
    /// rotation is observable as bounded runtime events.
    #[serde(default)]
    pub(crate) pending_ingest_transitions: Vec<ContextStateTransition>,
    /// Combined live hot set (non-expired tool-hot + user-hot). Rebuilt
    /// after every mutation so GC/scoring keep using one slice.
    #[serde(default)]
    pub(crate) hot_entities: Vec<String>,
    /// Entities named by the last user message or FocusChanged. Replaced
    /// on those events; not extended by tools.
    #[serde(default)]
    pub(crate) user_hot_entities: Vec<String>,
    /// Structured resource paths from WorkingSetSignal / file_path stamps.
    /// Short TTL; stdout never seeds this.
    #[serde(default)]
    pub(crate) tool_hot: Vec<ToolHotEntity>,
    /// Last-prompt file exposure. Cleared and rewritten each materialize;
    /// used for reread attribution, not GC policy. Selecting an item is
    /// not the same as packing its body — Checked identities pack as
    /// `path@rev`.
    #[serde(default)]
    pub(crate) selected_body_paths: HashSet<String>,
    #[serde(default)]
    pub(crate) selected_descriptor_paths: HashSet<String>,
    #[serde(default)]
    pub(crate) external_descriptor_paths: HashSet<String>,
    #[serde(default)]
    pub(crate) reread_previously_selected: u64,
    #[serde(default)]
    pub(crate) reread_selected_descriptor: u64,
    #[serde(default)]
    pub(crate) reread_external_descriptor: u64,
    #[serde(default)]
    pub(crate) reread_resident_unselected: u64,
    #[serde(default)]
    pub(crate) reread_warm: u64,
    #[serde(default)]
    pub(crate) reread_stored: u64,
    #[serde(default)]
    pub(crate) reread_first_read: u64,
    /// Copied from config at construction so `latest_file_body_ids` does
    /// not need the config on every GC/materialize call.
    #[serde(default = "default_recent_file_bodies")]
    pub(crate) recent_file_bodies: usize,
    #[serde(default)]
    pub(crate) recent_file_body_lease_turns: u64,
    /// Runtime scope tree: one session scope, one task scope per task, one
    /// focus scope per task while it runs, one tool scope per tool call.
    /// The tree owns its id index: `push`/`by_id`/`index_of` keep lookups
    /// O(1) and structural mutations cannot drift the index.
    #[serde(default)]
    pub(crate) scopes: crate::scope_tree::ScopeTree,
    /// Deepest scope currently receiving attention (tool > focus > task).
    #[serde(default)]
    pub(crate) active_scope_id: Option<ScopeId>,
    /// Scopes queued for close by ingest (task completion, tool result
    /// consumed); drained by maintenance so promotion/eviction is recorded.
    #[serde(default)]
    pub(crate) pending_closed_scopes: Vec<ScopeId>,
    /// Items evicted by the full GC pass. Bounded by
    /// `gc_buffer_capacity`; eviction is reversible — items re-enter the
    /// heap when they become roots again. Overflow externalizes to the
    /// context store (never purged).
    #[serde(default)]
    pub(crate) eviction_buffer: Vec<ContextItem>,
    /// Overflow items whose store write failed. They spill out of the
    /// bounded warm buffer into this retry list (content preserved, never
    /// absorbed back into the buffer past its cap); the next full GC pass
    /// retries them before taking new overflow from the buffer.
    #[serde(default)]
    pub(crate) pending_externalize_retry: Vec<ContextItem>,
    /// The external context map: lightweight entries for items whose content
    /// lives in the context store. `Cold` entries can still be recalled by
    /// hot-entity matches; `External` entries only exist as references. The
    /// map owns its id/entity indexes: structural mutations (push, retain,
    /// replace) go through `ExternalMap` methods so the indexes cannot drift.
    #[serde(default)]
    pub(crate) external: crate::index::external::ExternalMap,
    /// Canonical `item_id -> location` directory plus query indexes shared
    /// by GC recall and `context.search`. Derived from the three body stores
    /// and skipped in checkpoints; restore rebuilds it.
    #[serde(skip)]
    pub(crate) catalog: crate::index::catalog::ContextCatalog,
    /// Field edits (attention/semantic/tags) that do not go through heap
    /// or external structural methods. Drained into catalog sync.
    #[serde(skip)]
    pub(crate) catalog_dirty: crate::index::catalog::CatalogDirty,
    /// Bounded minor/aging resume points. Default batch covers the heaps
    /// used in tests, so a pass still visits every item in stable order.
    #[serde(default)]
    pub(crate) gc_cursor: crate::gc::GcWorkCursor,
    /// Counts full GC passes only. External-entry aging (Cold -> External)
    /// and TTLs compare this epoch, never the tick counter — the tick also
    /// advances on ingest/maintain/materialize, so a pass-based TTL must
    /// not drift with unrelated runtime activity.
    #[serde(default)]
    pub(crate) gc_epoch: u64,
    /// Cumulative GC counters, so diagnostics explain a run's eviction and
    /// reactivation behavior without replaying every report.
    #[serde(default)]
    pub(crate) gc_evicted_total: u64,
    #[serde(default)]
    pub(crate) gc_reactivated_total: u64,
    #[serde(default)]
    pub(crate) gc_externalized_total: u64,
    #[serde(default)]
    pub(crate) gc_storage_deleted_total: u64,
    /// 分级检索戳累计（进 diagnostics / checkpoint）。
    #[serde(default)]
    pub(crate) access_search_hits: u64,
    #[serde(default)]
    pub(crate) access_inspects: u64,
    #[serde(default)]
    pub(crate) access_fetches: u64,
    #[serde(default)]
    pub(crate) access_admits: u64,
    #[serde(default)]
    pub(crate) access_consumption_acks: u64,
    /// Foreground bodies consumed by successful model rounds (weak
    /// observational signal; never reinforces access or changes residency).
    #[serde(default)]
    pub(crate) foreground_consumed_acks: u64,
    /// Segment-local reactivation instrumentation. Skipped in checkpoints
    /// and zeroed on restore; run-global aggregation is event-side.
    #[serde(skip)]
    pub(crate) reactivation_selected: u64,
    #[serde(skip)]
    pub(crate) reactivation_consumed: u64,
    #[serde(skip)]
    pub(crate) reactivation_selected_tokens: u64,
    #[serde(skip)]
    pub(crate) reactivation_consumed_tokens: u64,
    #[serde(skip)]
    pub(crate) reactivation_events: u64,
    #[serde(skip)]
    pub(crate) unique_reactivated: u64,
    #[serde(skip)]
    pub(crate) reactivated_tokens: u64,
    #[serde(skip)]
    pub(crate) reactivation_tool_observation_selected: u64,
    #[serde(skip)]
    pub(crate) reactivation_tool_observation_consumed: u64,
    #[serde(skip)]
    pub(crate) reactivation_file_observation_selected: u64,
    #[serde(skip)]
    pub(crate) reactivation_file_observation_consumed: u64,
    #[serde(skip)]
    pub(crate) reactivation_traces: std::collections::HashMap<
        agent_contracts::ContextItemId,
        crate::reactivation::ReactivationTrace,
    >,
    /// 引擎自有蒸馏（任务完成）累计的压缩器花费。
    #[serde(default)]
    pub(crate) compaction_input_tokens: u64,
    #[serde(default)]
    pub(crate) compaction_output_tokens: u64,
    /// Compaction passes that have run since the last maintain drain.
    #[serde(skip)]
    pub(crate) pending_compactions: Vec<ContextCompaction>,
    /// Directives counted against the per-turn admit cap. Reset by the
    /// next user message (turn boundary); a turn whose admits are refused
    /// keeps the count so the model learns the cap from refusals.
    #[serde(default)]
    pub(crate) admits_this_turn: usize,
    /// Directives counted against the per-turn derive cap (same lifecycle
    /// as `admits_this_turn`).
    #[serde(default)]
    pub(crate) derives_this_turn: usize,
    /// 本回合已对相同检索指纹执行过 search 强化的次数。回合边界清零。
    /// 不进 checkpoint：恢复后最多再给一次 search 强化，不会变成 pin。
    #[serde(skip)]
    pub(crate) search_query_stamps_this_turn: std::collections::HashMap<u64, u32>,
    /// The active task's typed root claims, projected from its TaskAnchor by
    /// the runtime via `ContextAction::AnchorRoots`. Consumed by GC
    /// (`ResidentRequired`/`PromptRequired` protect the heap) and Storage GC
    /// (`StorageRequired` protects the store). Replacement-only, bounded by
    /// `MAX_ANCHOR_ROOT_CLAIMS`; the engine never owns task authority.
    #[serde(default)]
    pub(crate) anchor_roots: Vec<agent_contracts::AnchorRootClaim>,
    /// TaskProgress checked `path` / `path@revision` rows, projected by
    /// the runtime via `ContextAction::CheckedFiles` before GC. File-body
    /// auto-reactivation skips a path this set already covers. Not P3.
    #[serde(default)]
    pub(crate) checked_files: Vec<String>,
    /// Bounded in-engine lifecycle ledger: every item transition on any
    /// axis (attention/semantic/residency/gc) with cause, trigger, turn and
    /// related id. Oldest rows drop past the cap; export to a JSONL
    /// artifact is explicit and never on the hot path.
    #[serde(default)]
    pub(crate) ledger: Vec<agent_contracts::ContextLifecycleRecord>,
    /// Per-item revision counter backing `ContextLifecycleRecord::revision`.
    #[serde(default)]
    pub(crate) ledger_revisions: std::collections::HashMap<ContextItemId, u64>,
    /// Ledger buffer cap, copied from the config at construction so every
    /// record site only needs `&mut State`.
    #[serde(default)]
    pub(crate) ledger_cap: usize,
}

fn default_recent_file_bodies() -> usize {
    crate::index::entity::MAX_RECENT_FILE_BODIES
}

/// A tool-hot resource with a turn expiry. User-hot is unbounded until
/// the next user message replaces it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ToolHotEntity {
    pub entity: String,
    pub expires_at_turn: u64,
}

fn classify_fs_read(state: &State, path: &str) -> FsRereadClass {
    let path = normalize_resource_path(path);
    if path.is_empty() {
        return FsRereadClass::FirstRead;
    }
    let matches_item = |item: &ContextItem| {
        entity::observation_file_path(item)
            .is_some_and(|observed| normalize_resource_path(observed) == path)
    };
    let in_heap = state.items.iter().any(matches_item);
    let in_warm = state.eviction_buffer.iter().any(matches_item);
    let in_store = state.external.iter().any(|entry| {
        entry
            .entities
            .iter()
            .any(|entity| normalize_resource_path(entity) == path)
    });
    if !in_heap && !in_warm && !in_store {
        return FsRereadClass::FirstRead;
    }
    if state.selected_body_paths.contains(&path) {
        return FsRereadClass::PreviouslySelected;
    }
    if state.selected_descriptor_paths.contains(&path) {
        return FsRereadClass::SelectedDescriptor;
    }
    if state.external_descriptor_paths.contains(&path) {
        return FsRereadClass::ExternalDescriptor;
    }
    if in_heap {
        return FsRereadClass::ResidentUnselected;
    }
    if in_warm {
        return FsRereadClass::Warm;
    }
    FsRereadClass::Stored
}

impl State {
    /// Apply dirty catalog ids, or rebuild after a wholesale store replace.
    /// Search and GC recall consume this directory; callers must sync
    /// before reading it.
    pub(crate) fn sync_catalog(&mut self) {
        let (heap_rebuild, heap_dirty) = self.items.drain_catalog_dirty();
        let (ext_rebuild, ext_dirty) = self.external.drain_catalog_dirty();
        self.catalog_dirty.merge(heap_rebuild, heap_dirty);
        self.catalog_dirty.merge(ext_rebuild, ext_dirty);
        let dirty = std::mem::take(&mut self.catalog_dirty);
        self.catalog.sync(
            &self.items[..],
            &self.eviction_buffer,
            &self.pending_externalize_retry,
            &self.external[..],
            self.event_seq,
            dirty,
        );
    }

    pub(crate) fn mark_catalog(&mut self, id: ContextItemId) {
        self.catalog_dirty.mark(id);
    }

    pub(crate) fn mark_catalog_rebuild(&mut self) {
        self.catalog_dirty.mark_rebuild();
    }

    fn expire_tool_hot(&mut self) {
        self.tool_hot
            .retain(|entry| entry.expires_at_turn > self.turn);
    }

    pub(crate) fn rebuild_hot_entities(&mut self) {
        self.expire_tool_hot();
        // Start from user-hot, then prepend tool-hot (oldest-first into
        // merge_hot_entities so the newest tool path stays at the front).
        // Merging user-hot *after* tool-hot would bury operational paths.
        let mut combined = self.user_hot_entities.clone();
        let tool_oldest_first: Vec<String> = self
            .tool_hot
            .iter()
            .rev()
            .map(|entry| entry.entity.clone())
            .collect();
        entity::merge_hot_entities(&mut combined, tool_oldest_first);
        self.hot_entities = combined;
    }

    fn set_user_hot(&mut self, entities: Vec<String>) {
        self.user_hot_entities = entities;
        self.rebuild_hot_entities();
    }

    fn merge_tool_hot(&mut self, paths: impl IntoIterator<Item = String>, ttl_turns: u64) {
        let expires_at_turn = self.turn.saturating_add(ttl_turns.max(1));
        for path in paths {
            if path.is_empty() {
                continue;
            }
            if let Some(existing) = self.tool_hot.iter_mut().find(|entry| entry.entity == path) {
                existing.expires_at_turn = expires_at_turn;
                continue;
            }
            self.tool_hot.insert(
                0,
                ToolHotEntity {
                    entity: path,
                    expires_at_turn,
                },
            );
        }
        self.tool_hot.truncate(entity::MAX_HOT_ENTITIES);
        self.rebuild_hot_entities();
    }

    fn record_fs_reread(&mut self, class: FsRereadClass) {
        match class {
            FsRereadClass::PreviouslySelected => self.reread_previously_selected += 1,
            FsRereadClass::SelectedDescriptor => self.reread_selected_descriptor += 1,
            FsRereadClass::ExternalDescriptor => self.reread_external_descriptor += 1,
            FsRereadClass::ResidentUnselected => self.reread_resident_unselected += 1,
            FsRereadClass::Warm => self.reread_warm += 1,
            FsRereadClass::Stored => self.reread_stored += 1,
            FsRereadClass::FirstRead => self.reread_first_read += 1,
        }
    }

    /// 活跃任务里每个最近文件的最新成功观察。已完成任务或没有焦点时为空：
    /// 文件正文根不能把上一个任务的正文带进下一个任务。
    pub(crate) fn latest_file_body_ids(&self) -> std::collections::HashSet<ContextItemId> {
        let Some(task) = self.focus.as_ref().map(|focus| focus.task_id) else {
            return std::collections::HashSet::new();
        };
        let completed = self.scopes.iter().any(|scope| {
            scope.kind == ScopeKind::Task
                && scope.task_id == Some(task)
                && scope.state == ScopeState::Closed
        });
        if completed {
            return std::collections::HashSet::new();
        }
        entity::latest_file_body_ids(
            self.items.iter(),
            Some(task),
            if self.recent_file_bodies == 0 {
                entity::MAX_RECENT_FILE_BODIES
            } else {
                self.recent_file_bodies
            },
            self.recent_file_body_lease_turns,
            self.turn,
        )
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct IoBoundaryPause {
    pub(crate) planned: Arc<tokio::sync::Notify>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

pub struct SimpleContextEngine {
    pub(crate) config: SimpleContextConfig,
    pub(crate) state: Mutex<State>,
    /// Serializes the multi-phase and whole-state operations. GC, storage
    /// GC, store reconcile, checkpoint and restore each span several state
    /// lock acquisitions (deliberately releasing the state lock across disk
    /// IO); the gate keeps them from interleaving, so a plan computed
    /// against one state can never be committed against a state another
    /// operation replaced in between. Materialization, incomplete Stored
    /// search, external Admit and Fetch also take the gate because they
    /// snapshot store ownership, read without the lock, then commit or return
    /// that result. Ingest and maintenance share this mutation lane so they
    /// cannot terminalize or move an owner inside another operation's unlocked
    /// I/O window. Lock order is always gate, then state.
    pub(crate) op_gate: Mutex<()>,
    /// Deterministic plan/I/O boundary used only by concurrency regressions.
    #[cfg(test)]
    pub(crate) materialize_io_pause: std::sync::Mutex<Option<IoBoundaryPause>>,
    /// Deterministic pause at the admit external-read boundary (same shape
    /// as `materialize_io_pause`): the planned store read runs with the
    /// state lock released, and regressions prove unrelated work completes
    /// while the read is parked here — no timing inference.
    #[cfg(test)]
    pub(crate) admit_read_pause: std::sync::Mutex<Option<IoBoundaryPause>>,
    /// Deterministic pause at the checkpoint card-write boundary (F2): the
    /// planned cards are written with the state lock released, and a
    /// regression proves unrelated state reads answer while the capture is
    /// parked here — no timing inference.
    #[cfg(test)]
    pub(crate) checkpoint_io_pause: std::sync::Mutex<Option<IoBoundaryPause>>,
    /// N02 regression gate: park a hydration exactly at the n-th card-read
    /// boundary (0-based index within one batch's read plan), so a dropped
    /// future is observed against a precise mid-read state — no timing
    /// inference.
    #[cfg(test)]
    pub(crate) card_read_pause: std::sync::Mutex<Option<(IoBoundaryPause, usize)>>,
    /// N02 regression fault: when nonzero, the next card read fails with a
    /// fabricated transient I/O error (the value bounds how many reads).
    #[cfg(test)]
    pub(crate) card_read_failure_bomb: std::sync::atomic::AtomicU32,
    /// 与 B 共用的有界压缩器。缺省为 None：任务摘要仍用 runtime 给的原文。
    /// 注入后，任务完成和 episode 旋转会蒸馏成带 `DerivedFrom` 的派生摘要，
    /// 原文条目保留。
    compactor: Option<Arc<dyn BoundedCompactor>>,
    /// Last catalog search's Stored-body I/O. Not checkpointed.
    search_observation: std::sync::Mutex<ContextSearchObservation>,
    /// S3: typed candidate coverage of the last search — the model-facing
    /// facts (complete / unread pages / stop cause / continuation token).
    /// Not checkpointed.
    last_search_coverage: std::sync::Mutex<ContextSearchCoverage>,
    /// S3: the cold-page continuation this engine issued for its most recent
    /// incomplete search: the opaque token, the normalized query it is bound
    /// to, and the hot ids that window covered. Not checkpointed — a
    /// restored engine has no continuation until its next incomplete search
    /// issues one (V5: an in-place `restore` enforces this by clearing the
    /// slot and bumping the restore generation).
    search_continuation: std::sync::Mutex<Option<IssuedSearchContinuation>>,
    /// W2 (V5): process-lifetime monotonic serial stamped into every
    /// continuation token. It never resets — not on chain completion, not on
    /// a new query, not on restore — so a completed chain's token identity
    /// can never be re-derived by a later chain (the old slot-suffix
    /// numbering restarted at 1 and ABA-matched across chains).
    continuation_serial: std::sync::atomic::AtomicU64,
    /// W2 (V5): restore generation. Every successful in-place restore bumps
    /// it; a token also embeds the generation it was issued under, so a
    /// pre-restore token can never validate against a post-restore walk even
    /// if the slot itself were repopulated. A rejected restore leaves it —
    /// and the live chain — untouched.
    continuation_epoch: std::sync::atomic::AtomicU64,
}

/// S3: one issued search continuation. The token is opaque to callers; the
/// engine validates token + query binding (and, since V5, the restore
/// generation) before rotating the window, so a stale or foreign token
/// degrades into a fresh search instead of moving a wrong window.
#[derive(Debug, Clone)]
struct IssuedSearchContinuation {
    token: String,
    query_key: String,
    /// V5: the restore generation this chain was issued under.
    epoch: u64,
    /// V4: the walk's accumulated covered set, deduped and bounded by
    /// [`SimpleContextConfig::search_continuation_max_covered_ids`].
    covered_ids: Vec<ContextItemId>,
}

impl SimpleContextEngine {
    pub fn new(config: SimpleContextConfig) -> Self {
        let state = State {
            ledger_cap: config.max_ledger_records,
            recent_file_bodies: config.recent_file_bodies,
            recent_file_body_lease_turns: config.recent_file_body_lease_turns,
            ..State::default()
        };
        Self {
            config,
            state: Mutex::new(state),
            op_gate: Mutex::new(()),
            #[cfg(test)]
            materialize_io_pause: std::sync::Mutex::new(None),
            #[cfg(test)]
            admit_read_pause: std::sync::Mutex::new(None),
            #[cfg(test)]
            checkpoint_io_pause: std::sync::Mutex::new(None),
            #[cfg(test)]
            card_read_pause: std::sync::Mutex::new(None),
            #[cfg(test)]
            card_read_failure_bomb: std::sync::atomic::AtomicU32::new(0),
            compactor: None,
            search_observation: std::sync::Mutex::new(ContextSearchObservation::default()),
            last_search_coverage: std::sync::Mutex::new(ContextSearchCoverage::complete()),
            search_continuation: std::sync::Mutex::new(None),
            continuation_serial: std::sync::atomic::AtomicU64::new(0),
            continuation_epoch: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn with_compactor(mut self, compactor: Arc<dyn BoundedCompactor>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    fn record_search_observation(&self, observation: ContextSearchObservation) {
        match self.search_observation.lock() {
            Ok(mut slot) => *slot = observation,
            Err(poisoned) => *poisoned.into_inner() = observation,
        }
    }

    /// The normalized query identity a continuation is bound to: the same
    /// text and filters must come back for a token to rotate its window.
    /// `limit` is deliberately not part of the identity — it caps how much
    /// of a page the model displays, not which region the walk covers.
    fn search_query_key(query: &agent_contracts::ContextSearchQuery) -> String {
        format!(
            "q={:?}|kind={:?}|scope={:?}|task={:?}|label={:?}",
            query.query, query.kind, query.scope, query.task_id, query.label
        )
    }

    /// One catalog search, optionally resumed at a previous pass's cold-page
    /// window.
    ///
    /// Enforces the semantic boundary here as well as in Core. Direct engine
    /// and sidecar callers must not bypass the same output/query bounds the
    /// model-facing path uses; zero keeps the engine default.
    ///
    /// S3 (R4): the drain outcome no longer disappears behind
    /// `complete || !hits.is_empty()`. Every pass records typed coverage and
    /// — while pages remain unread — a query-bound continuation token; a
    /// valid token rotates the covered hot window out to the pending
    /// directory first, so the drain pages in the NEXT cold region instead
    /// of re-serving the same pages forever.
    ///
    /// W2 (V3): the pass returns ATOMICALLY — hits, coverage and observation
    /// describe exactly this pass, so callers (including the service
    /// boundary) never re-read a mutable "last search" channel afterwards.
    async fn search_report_with_continuation(
        &self,
        query: agent_contracts::ContextSearchQuery,
        continuation: Option<&str>,
    ) -> AgentResult<agent_contracts::ContextSearchResult> {
        let query = query.normalized();
        let query_key = Self::search_query_key(&query);
        // Search stamps access state even when no body read is needed, so the
        // whole operation shares the mutation lane. An incomplete catalog may
        // additionally release the state lock for checked Stored-body reads.
        let _gate = self.op_gate.lock().await;
        let mut skip: Vec<ContextItemId> = Vec::new();
        // V4: only a token the rotation actually validated makes this pass a
        // RESUME of an existing walk. A missing, stale, foreign or
        // pre-restore token leaves `resumed` false and the pass runs FRESH:
        // it inherits nothing from any older walk.
        let mut resumed = false;
        if let Some(token) = continuation
            && let Some(ids) = self.rotate_search_window(&query_key, token).await
        {
            skip = ids;
            resumed = true;
        }
        // F2: search coverage is unchanged by restore paging — a pending
        // spill row is paged in first (bounded batches) so the catalog and
        // the residual scan see the same external set they always did.
        //
        // B2: the drain outcome decides whether an empty result may be
        // reported. A row left unread is a cold entry whose body the catalog
        // cannot see; an empty hit list in that state is NOT a complete
        // zero-match, so it fails closed below instead.
        //
        // T4: the drain is budget-bounded, and the typed outcome (unread
        // remainder + stop cause) is what `finish_search_report` consumes —
        // same fail-closed rule, now naming the resumable amount.
        //
        // S3: under a continuation the drain skips the pages the rotation
        // just re-queued, so the walk terminates at the last unseen page
        // instead of cycling over already-covered windows forever.
        let hydration = self.hydrate_pending_cards_within_budget(&skip).await;
        let read_plan = {
            let mut state = self.state.lock().await;
            state.sync_catalog();
            let incomplete = state
                .catalog
                .search_candidates(&query)
                .and_then(|candidates| candidates.incomplete);
            if incomplete.is_none() {
                let hits = crate::store::search_catalog(&state, &query);
                crate::access::reinforce_search_hits(&mut state, &hits, &query);
                drop(state);
                let observation = ContextSearchObservation::default();
                self.record_search_observation(observation);
                return self
                    .finish_search_report(query_key, resumed, hydration, hits, observation)
                    .await;
            }
            let read_plan = crate::store::plan_stored_search_reads(&state, &query)?;
            if read_plan.is_empty() {
                let hits = crate::store::search_catalog(&state, &query);
                crate::access::reinforce_search_hits(&mut state, &hits, &query);
                drop(state);
                let observation = ContextSearchObservation::default();
                self.record_search_observation(observation);
                return self
                    .finish_search_report(query_key, resumed, hydration, hits, observation)
                    .await;
            }
            read_plan
        };
        let dir = crate::store::store_dir(&self.config);
        let started = std::time::Instant::now();
        let verification =
            crate::store::verify_stored_search_reads(&dir, read_plan, &query).await?;
        let observation = ContextSearchObservation {
            cold_reads: verification.read_count,
            cold_read_bytes: verification.read_bytes,
            cold_read_ms: started.elapsed().as_millis() as u64,
        };
        let mut state = self.state.lock().await;
        state.sync_catalog();
        let hits = crate::store::search_catalog_with_verified_bodies(
            &state,
            &query,
            &verification.matched,
        );
        // search 命中是最弱信号：相同查询本回合只强化一次，单条目同一
        // event_seq 冷却，饱和后不再推迟 Cold 老化。terminal 命中已被
        // externally_retrievable 过滤；search 从不覆盖终态语义或 GC 根。
        crate::access::reinforce_search_hits(&mut state, &hits, &query);
        drop(state);
        self.record_search_observation(observation);
        self.finish_search_report(query_key, resumed, hydration, hits, observation)
            .await
    }

    /// Engine-owned focused task. Restore alignment and tests read this;
    /// production `materialize` leaves `MaterializedContext.focus` empty.
    pub async fn focused_task_id(&self) -> Option<agent_contracts::TaskId> {
        self.state
            .lock()
            .await
            .focus
            .as_ref()
            .map(|focus| focus.task_id)
    }

    pub async fn focused_goal(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .focus
            .as_ref()
            .map(|focus| focus.goal.clone())
    }

    async fn run_distill(&self, job: &DistillJob) -> CompactionOutput {
        let fallback = bound_compaction_output(&job.fallback);
        let Some(compactor) = &self.compactor else {
            return CompactionOutput {
                text: fallback,
                ..CompactionOutput::default()
            };
        };
        match compactor
            .compact(CompactionRequest {
                folded_items: job.source_ids.len(),
                source: job.source.clone(),
            })
            .await
        {
            Ok(mut output) => {
                output.text = bound_compaction_output(&output.text);
                if output.text.is_empty() {
                    output.text = fallback;
                }
                output
            }
            // COST-7 (R2-11): the call failed, but evidence the provider
            // already reported (an empty-summary error carries the billed
            // call's usage) stays in the account under its honest identity
            // — a partially-reported call degrades to Unknown with the
            // known values as the lower bound, never a fabricated 0/0.
            Err(error) => match error.reported_usage() {
                Some(usage) => CompactionOutput {
                    text: fallback,
                    input_tokens: usage.input_tokens.unwrap_or(0),
                    output_tokens: usage.output_tokens.unwrap_or(0),
                    usage_identity: usage.usage_identity(),
                    cached_input_tokens: usage.cached_input_tokens,
                    cache_write_input_tokens: usage.cache_write_input_tokens,
                    cache_miss_input_tokens: usage.cache_miss_input_tokens,
                    attempts: usage.attempts,
                    retries: usage.retries,
                },
                None => CompactionOutput {
                    text: fallback,
                    // No typed evidence at all: whether the provider
                    // processed (and billed) work before failing is
                    // unknowable.
                    usage_identity: UsageIdentity::Unknown,
                    ..CompactionOutput::default()
                },
            },
        }
    }

    /// Export the bounded in-engine lifecycle ledger to a JSONL artifact
    /// (one `ContextLifecycleRecord` per line) and clear the buffer. This
    /// is an explicit, off-hot-path operation; a crashed export never
    /// truncates the previous artifact (temp file + rename).
    pub async fn export_ledger(&self, path: &std::path::Path) -> AgentResult<usize> {
        // Taking and later restoring rows is one mutation even though the
        // artifact write runs without the state lock. Serialize it with
        // restore and every other state-changing operation.
        let _gate = self.op_gate.lock().await;
        // B3: SNAPSHOT the rows; do not remove them yet. The previous shape did
        // `mem::take` and only merged the rows back on an I/O error, so a
        // cancelled export — the future dropped while it awaited the write or
        // the rename — never reached that error branch and the rows went with
        // the local variable. Committing first and confirming the consumption
        // afterwards is what makes the removal happen only for an artifact that
        // actually exists.
        let rows: Vec<agent_contracts::ContextLifecycleRecord> = {
            let state = self.state.lock().await;
            state.ledger.to_vec()
        };
        if rows.is_empty() {
            return Ok(0);
        }
        let text = crate::ledger::encode(&rows);
        let tmp = path.with_extension("jsonl.tmp");
        // The temp write and rename run as async IO rather than blocking calls.
        // A failure here changes nothing: the buffer still owns every row.
        tokio::fs::write(&tmp, text).await.map_err(|error| {
            agent_contracts::AgentError::Storage(format!("write ledger artifact: {error}"))
        })?;
        tokio::fs::rename(&tmp, path).await.map_err(|error| {
            agent_contracts::AgentError::Storage(format!("commit ledger artifact: {error}"))
        })?;
        // The artifact is committed. Now, and only now, consume exactly the
        // rows it carries.
        let consumed = {
            let mut state = self.state.lock().await;
            crate::ledger::confirm_exported(&mut state, &rows)
        };
        Ok(consumed)
    }

    /// F2 phase 2 of a capture: put the planned cards on disk with the state
    /// lock released. Existence probes cost no write budget (a card is
    /// content-addressed, so an existing file already holds these bytes);
    /// the wall-clock budget stops a slow disk from stretching one capture,
    /// and the entries it leaves behind simply stay inline this time.
    async fn run_external_spill_io(&self, plan: ExternalSpillPlan) -> ExternalSpillIo {
        let mut io = ExternalSpillIo {
            spilled: plan.recorded,
            ..ExternalSpillIo::default()
        };
        if plan.writes.is_empty() {
            return io;
        }
        #[cfg(test)]
        {
            let pause = self
                .checkpoint_io_pause
                .lock()
                .expect("checkpoint test pause mutex poisoned")
                .clone();
            if let Some(pause) = pause {
                pause.planned.notify_one();
                pause.release.notified().await;
            }
        }
        let dir = crate::store::store_dir(&self.config);
        let budget = std::time::Duration::from_millis(self.config.external_checkpoint_io_budget_ms);
        let started = std::time::Instant::now();
        for (index, (item_id, hash, bytes)) in plan.writes.into_iter().enumerate() {
            let path = crate::store::external_card_path(&dir, item_id, &hash);
            // B2（首次认领校验）：路径名里的哈希由本次计划的字节导出，所以
            // 已存在的文件只有读回与计划字节一致才是这张卡的有效 claim
            // （内容寻址幂等，免重写）。同名异字节（崩溃残片、截断、异物、
            // 目录占位）不可认领——认领会让 checkpoint 把坏引用记进
            // manifest 并把条目排出 inline 段，恢复时才发现。长度不同的
            // 文件不可能一致，不做整读；可读但不一致或不可读时走下面的
            // 安全原子写入（修复或首次写入），写入失败（目录占位、权限、
            // 磁盘故障）保持内联——宁可 checkpoint 大，不可记坏引用。
            let existing_matches = match tokio::fs::metadata(&path).await {
                Ok(meta) if meta.is_file() && meta.len() == bytes.len() as u64 => {
                    tokio::fs::read(&path)
                        .await
                        .is_ok_and(|existing| existing == bytes)
                }
                _ => false,
            };
            if existing_matches {
                io.written.push((item_id, hash.clone()));
                io.spilled.push((item_id, hash));
                continue;
            }
            // Checked after the first write so a spent budget still makes
            // progress instead of live-locking the spill forever. What is
            // left stays inline; the next capture continues.
            if index > 0 && started.elapsed() >= budget {
                break;
            }
            match crate::store::write_external_card_async(&dir, &path, &bytes).await {
                Ok(()) => {
                    io.written.push((item_id, hash.clone()));
                    io.spilled.push((item_id, hash));
                }
                // Store IO 失败：该条目本次保持内联（宁可 checkpoint 大，
                // 不可丢恢复状态）；与外部化失败同一诚实语义。
                Err(_) => continue,
            }
        }
        io
    }

    /// F2: page in at most `take` pending spill cards, under the operation's
    /// remaining [`HydrationBudget`]. Restore leaves the tail of a long
    /// history as `(id, card hash)` rows; this is the bounded drain every
    /// completeness-sensitive caller runs before it needs the whole external
    /// set. A row whose id was claimed in the meantime is dropped without
    /// touching the live owner, and a missing, corrupt or structurally
    /// invalid card is consumed and counted exactly like a missing card at
    /// restore time.
    ///
    /// N02: the pending rows keep their owner until a read *verifies*. The
    /// lock-held phase only copies a bounded read plan, so a future dropped
    /// between the plan and the commit (cancellation at any await boundary)
    /// loses nothing — the rows are still queued and the next batch reads
    /// them again. Only a verified outcome migrates a row off the queue: an
    /// installed entry, or a permanently absent/damaged card. A transient
    /// I/O failure keeps its row for the next drain — it is never
    /// equivalent to "the data does not exist".
    ///
    /// S3 (deadline): each read is awaited under the budget's *remaining*
    /// deadline — a read that outlives it is cancelled at the wait boundary
    /// (the row keeps its pending owner, nothing half-installed is ever
    /// committed) and the batch reports `timed_out`. This bounds the
    /// operation's wall clock at the read granularity, not only between
    /// batches; the underlying disk I/O itself is not claimed to be
    /// physically cancelled.
    ///
    /// S3 (byte reservation): before an entry installs, its
    /// `entry_metadata_bytes_estimate` is reserved against the remaining
    /// byte room (same weights the map-level estimate sums — the estimate
    /// bounds the metadata footprint, not RSS), and against the remaining
    /// entry room. An entry that fits neither stays a queued, addressable
    /// cold owner (`oversized`) — metadata is never dropped to keep a cap.
    ///
    /// S3 (rotation): when a batch installed nothing and every read hit a
    /// transient failure while rows remain behind the failed window, the
    /// failed rows move to the back of the queue (`rotated`). Their owners
    /// are untouched; this only stops an unreadable front row from
    /// permanently blocking later readable pages.
    pub(crate) async fn hydrate_pending_cards(
        &self,
        take: usize,
        budget: &HydrationBudget,
        skip: &[ContextItemId],
    ) -> PendingCardBatch {
        let rows: Vec<(ContextItemId, String)> = {
            let state = self.state.lock().await;
            if state.pending_external_cards.is_empty() {
                return PendingCardBatch::default();
            }
            // S3: a continuation drain reads only rows its walk has not
            // already covered — the skip set marks the pages a previous
            // rotation re-queued, and the window stops at the first one so
            // seen pages are never re-read before unseen ones.
            state
                .pending_external_cards
                .iter()
                .take_while(|(id, _)| !skip.contains(id))
                .take(take)
                .cloned()
                .collect()
        };
        let dir = crate::store::store_dir(&self.config);
        let mut found: Vec<(ContextItemId, String, agent_contracts::ExternalizedContext)> =
            Vec::new();
        let mut consumed: Vec<ContextItemId> = Vec::new();
        let mut missing = 0u64;
        let mut io_failures = 0u64;
        let mut batch = PendingCardBatch::default();
        let mut failed_rows: Vec<(ContextItemId, String)> = Vec::new();
        for (index, (item_id, hash)) in rows.iter().enumerate() {
            let remaining = budget
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                // Every row not yet read this batch shares the timed-out
                // fate: the operation's clock is spent.
                batch.timed_out += rows.len() - index;
                break;
            }
            match tokio::time::timeout(
                remaining,
                self.read_card_with_test_hooks(&dir, *item_id, hash, index),
            )
            .await
            {
                Ok(crate::store::ExternalCardRead::Found(entry)) => {
                    found.push((*item_id, hash.clone(), entry));
                }
                Ok(crate::store::ExternalCardRead::Missing)
                | Ok(crate::store::ExternalCardRead::Corrupt(_)) => {
                    consumed.push(*item_id);
                    missing += 1;
                }
                Ok(crate::store::ExternalCardRead::IoFailed(_)) => {
                    io_failures += 1;
                    failed_rows.push((*item_id, hash.clone()));
                }
                Err(_elapsed) => {
                    // The read outlived the remaining deadline. This row
                    // keeps its pending owner; the batch ends here.
                    batch.timed_out += 1;
                    batch.timed_out += rows.len() - index - 1;
                    break;
                }
            }
        }
        let mut state = self.state.lock().await;
        let mut claimed = Vec::new();
        // Invariant: after this loop, the sum of the installed entries'
        // estimates never exceeds the byte room this batch started with, so
        // the post-install map estimate stays under the configured cap. The
        // estimate is the fixed `entry_metadata_bytes_estimate` weight set
        // (256/entry + 64/dependency + uri/summary/entity lengths) — it
        // bounds metadata residency, not RSS.
        let mut byte_room = budget
            .hot_max_bytes
            .saturating_sub(state.external.metadata_bytes_estimate());
        let mut entry_room = budget.hot_max_entries.saturating_sub(state.external.len());
        for (item_id, hash, entry) in found {
            // Someone else may own this id now (a reconcile rebuild, an
            // admit). The live owner wins; a paged-in card never creates a
            // second owner. B2: the read verified and the row's promise is
            // already fulfilled, so the redundant row is consumed — leaving
            // it queued would make every later drain re-read this card
            // forever and report the external set as incomplete, deferring
            // deletions indefinitely.
            if state.external.get(item_id).is_some()
                || crate::store::catalog_body(&state, item_id).is_some()
            {
                state
                    .pending_external_cards
                    .retain(|(id, _)| *id != item_id);
                continue;
            }
            // N03: a card whose entry references a scope this state does not
            // know is structurally invalid — the same violation
            // `checkpoint::validate` rejects at restore time. It must not
            // install; the card file stays on disk (the locator stays
            // diagnosable) while the row is consumed like a corrupt card.
            if let Some(scope_id) = entry.scope_id
                && state.scopes.by_id(scope_id).is_none()
            {
                consumed.push(item_id);
                missing += 1;
                continue;
            }
            // S3: reserve the entry's estimated metadata bytes and one entry
            // slot before installing. An entry that fits no remaining room
            // keeps its queued row — an addressable cold owner, never a
            // dropped locator.
            let estimate = crate::index::external::entry_metadata_bytes_estimate(&entry);
            if entry_room == 0 || estimate > byte_room {
                batch.oversized += 1;
                continue;
            }
            // N02: the row must still be the pending owner under the same
            // hash for this read to migrate it.
            let Some(position) = state
                .pending_external_cards
                .iter()
                .position(|(id, h)| *id == item_id && *h == hash)
            else {
                continue;
            };
            state.pending_external_cards.remove(position);
            byte_room -= estimate;
            entry_room -= 1;
            claimed.push((entry, hash));
        }
        if !consumed.is_empty() {
            let consumed: HashSet<ContextItemId> = consumed.into_iter().collect();
            state
                .pending_external_cards
                .retain(|(id, _)| !consumed.contains(id));
        }
        batch.installed = claimed.len();
        batch.consumed_missing = missing as usize;
        batch.io_failed = io_failures as usize;
        state
            .external
            .merge_paged(claimed.iter().map(|(entry, _)| entry.clone()).collect());
        for (entry, hash) in claimed {
            state.external.record_card(entry.item_id, hash);
        }
        state.external_cards_missing = state.external_cards_missing.saturating_add(missing);
        state.external_card_io_failures =
            state.external_card_io_failures.saturating_add(io_failures);
        // S3 rotation: nothing installed and nothing consumed while rows
        // remain behind the failed window — move the failed rows to the back
        // so the next take reaches pages after them. Owners stay queued the
        // whole time; only their position changes.
        if batch.installed == 0
            && batch.consumed_missing == 0
            && !failed_rows.is_empty()
            && state.pending_external_cards.len() > failed_rows.len()
        {
            let failed: HashSet<ContextItemId> = failed_rows.iter().map(|(id, _)| *id).collect();
            state
                .pending_external_cards
                .retain(|(id, _)| !failed.contains(id));
            state.pending_external_cards.extend(failed_rows);
            batch.rotated = failed.len();
        }
        state.sync_catalog();
        batch
    }

    /// S3 (renamed from `hydrate_all_pending_cards`, which no longer
    /// promised to read every pending card): page pending spill rows in
    /// under this operation's budget. Search, GC planning, reconcile and
    /// storage GC run this before they need the external set; the typed
    /// outcome tells them how much stayed unread.
    ///
    /// The budget (items + absolute deadline + resident-metadata caps, from
    /// config) stops the drain between read batches and at every single read
    /// boundary; the pending queue is never dropped, so the next operation
    /// resumes exactly where this one stopped.
    ///
    /// The rules this propagates (see `plan_storage_gc`):
    /// - a pending owner is not an ownerless entry;
    /// - recovery-root completeness is not metadata/dependency completeness;
    /// - while completeness is unknown, irreversible deletion defers.
    async fn hydrate_pending_cards_within_budget(
        &self,
        skip: &[ContextItemId],
    ) -> HydrationOutcome {
        let budget = HydrationBudget::for_operation(&self.config);
        self.hydrate_within_budget(budget, skip).await
    }

    /// T4: the budgeted drain body. Visible to tests so regressions can pin
    /// exact budget arithmetic; production callers go through
    /// [`Self::hydrate_pending_cards_within_budget`].
    pub(crate) async fn hydrate_within_budget(
        &self,
        budget: HydrationBudget,
        skip: &[ContextItemId],
    ) -> HydrationOutcome {
        let batch = self.config.external_restore_card_batch.max(1);
        let mut items_left = budget.max_items;
        loop {
            let (unread_len, pressure) = {
                let state = self.state.lock().await;
                let unread_len = state
                    .pending_external_cards
                    .iter()
                    .filter(|(id, _)| !skip.contains(id))
                    .count();
                (
                    unread_len,
                    MetadataResidencyPressure::measure(
                        &state.external,
                        budget.hot_max_entries,
                        budget.hot_max_bytes,
                    ),
                )
            };
            if unread_len == 0 {
                // S3: every remaining row is a page this operation's walk
                // already covered — the candidate region was examined whole.
                return HydrationOutcome::complete();
            }
            // T4: the resident-metadata caps stop the *bulk* drain only. The
            // per-id service path (fetch/inspect/a directive naming its own
            // target) never goes through here, so a capped hot directory
            // still serves any pending row the model asks for by id — and
            // settles its own residency through `settle_metadata_residency`.
            if pressure.entries >= budget.hot_max_entries
                || pressure.bytes_estimate >= budget.hot_max_bytes
            {
                return HydrationOutcome::stopped(HydrationStop::HotCap, unread_len);
            }
            // The caps bound the batch, so a drain can fill the hot
            // directory exactly to the cap without stepping past it.
            let room = budget.hot_max_entries - pressure.entries;
            let take = batch.min(items_left).min(room);
            if take == 0 || std::time::Instant::now() >= budget.deadline {
                return HydrationOutcome::stopped(HydrationStop::Budget, unread_len);
            }
            items_left -= take;
            let settled = self.hydrate_pending_cards(take, &budget, skip).await;
            // S3: a read cancelled at the deadline boundary ends the drain
            // with the typed timeout cause; the unread rows keep their
            // owners.
            if settled.timed_out > 0 {
                return HydrationOutcome::stopped(HydrationStop::Deadline, unread_len);
            }
            // N02: a batch that installs nothing and consumes nothing hit
            // only transiently unreadable cards — their rows stayed queued
            // on purpose. Draining them is a later successful pass's job;
            // spinning here would turn one flaky read into an unbounded
            // retry loop.
            let remaining = {
                let state = self.state.lock().await;
                state
                    .pending_external_cards
                    .iter()
                    .filter(|(id, _)| !skip.contains(id))
                    .count()
            };
            if settled.installed == 0 && remaining == unread_len {
                if settled.rotated > 0 {
                    // S3: the unreadable front rows moved behind the window;
                    // the next batch reads pages after them. Progress is the
                    // window moving, not an install.
                    continue;
                }
                if settled.oversized > 0 {
                    // S3: the remaining room fits none of the read entries'
                    // metadata estimates. They stay queued and addressable;
                    // the drain reports the honest capacity stop instead of
                    // over-filling the hot map.
                    return HydrationOutcome::stopped(HydrationStop::HotCap, remaining);
                }
                // B2: the batch hit only unreadable cards. Report the drain
                // as incomplete instead of letting the caller plan as if
                // the partial external set were the whole one.
                return HydrationOutcome::stopped(HydrationStop::Unreadable, remaining);
            }
        }
    }

    /// B2: report search hits — unless the result is empty while pending
    /// spill pages were left unread this pass. In that state the catalog
    /// could not see those cold bodies at all, so an empty `Ok` would read
    /// as a complete zero-match over the whole external set. Same fail-closed
    /// rule as the checked stored-read phase: a coverage failure is surfaced
    /// as a typed error, never folded into an authoritative-looking "no
    /// matches". Partial hits are returned as-is; the caller-facing search
    /// is bounded and ranked, so non-empty results never claimed completeness.
    ///
    /// T4: the fail-closed error carries the typed drain outcome — the exact
    /// unread remainder and why the drain stopped (budget spent, hot cap, or
    /// transient read failures) — so a caller can distinguish "no matches in
    /// the region we could see" from a resumable coverage gap.
    ///
    /// S3 (R4): every search records typed coverage
    /// ([`Self::last_search_coverage`]) — `complete || !hits.is_empty()`
    /// alone is no longer the only channel. An incomplete pass also issues a
    /// continuation token bound to this query, so the model-visible result
    /// can say "only part of the catalog was examined" and name the way to
    /// reach the next cold page. Zero hits over an unread region still fail
    /// closed (the token rides in the error text).
    async fn finish_search_report(
        &self,
        query_key: String,
        resumed: bool,
        hydration: HydrationOutcome,
        hits: Vec<agent_contracts::ExternalizedContext>,
        observation: ContextSearchObservation,
    ) -> AgentResult<agent_contracts::ContextSearchResult> {
        let coverage = self
            .record_search_coverage(query_key, resumed, hydration)
            .await;
        if hydration.complete || !hits.is_empty() {
            // W2 (V3): the atomic result — the pass's hits, coverage facts and
            // observation travel together.
            return Ok(agent_contracts::ContextSearchResult {
                hits,
                coverage,
                observation,
            });
        }
        let continuation_hint = coverage
            .continuation
            .as_deref()
            .map(|token| {
                format!(
                    "; rerun the same search with continuation=\"{token}\" to advance to the \
                     next cold page"
                )
            })
            .unwrap_or_default();
        Err(AgentError::Context(format!(
            "context search coverage incomplete: {} pending spill page(s) could not be paged in \
             this pass (stopped: {}) and may contain matches; reporting an empty result would \
             be a false zero-match{continuation_hint}",
            hydration.remaining, coverage.stop
        )))
    }

    /// S3 + W2 (V4): turn one drain outcome into the caller-visible coverage
    /// facts and (while pages remain unread) issue the next window
    /// continuation. The covered set of a RESUMED walk is the deduped union
    /// of the walk's accumulated prefix and the carded, non-pinned hot ids
    /// this pass could see; a FRESH pass (no token, or a token the rotation
    /// rejected) starts a new walk that inherits nothing — so repeating a
    /// plain search over a fixed history cannot grow the retained state.
    async fn record_search_coverage(
        &self,
        query_key: String,
        resumed: bool,
        hydration: HydrationOutcome,
    ) -> ContextSearchCoverage {
        if hydration.complete {
            let coverage = ContextSearchCoverage::complete();
            *self
                .last_search_coverage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = coverage.clone();
            // The queue drained: an older token has nothing left to advance
            // to and must not rotate a fresh window. The token serial keeps
            // counting (V5): the next chain mints fresh identities.
            *self
                .search_continuation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            return coverage;
        }
        let window_ids = {
            let state = self.state.lock().await;
            state.external.carded_hot_ids()
        };
        let mut slot = self
            .search_continuation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // V4 fresh/resume split. `resumed` is true only when the rotation
        // validated the token against THIS slot under op_gate, so the match
        // arm below re-checks the binding defensively, not as the authority.
        // A fresh pass replaces the walk; a resume extends it with dedup.
        let covered = match (resumed, slot.as_ref()) {
            (true, Some(issued))
                if issued.query_key == query_key
                    && issued.epoch
                        == self
                            .continuation_epoch
                            .load(std::sync::atomic::Ordering::Relaxed) =>
            {
                Self::merge_covered_dedup(&issued.covered_ids, window_ids)
            }
            _ => Self::merge_covered_dedup(&[], window_ids),
        };
        // V4 chain-level bound: the walk's accumulated set is engine-retained
        // state, so it has an explicit ceiling. Past it the chain CLOSES —
        // the set is released, the pass reports the typed stop with no
        // continuation, and the only way forward is a fresh search (which
        // the caller can issue immediately). `ContextItemId` is fixed-size,
        // so the entry count is the byte bound as well.
        if covered.len() > self.config.search_continuation_max_covered_ids {
            *slot = None;
            let coverage = ContextSearchCoverage {
                complete: false,
                unread_pages: hydration.remaining,
                stop: ContextSearchCoverageStop::Budget,
                continuation: None,
            };
            *self
                .last_search_coverage
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = coverage.clone();
            return coverage;
        }
        let serial = self
            .continuation_serial
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let epoch = self
            .continuation_epoch
            .load(std::sync::atomic::Ordering::Relaxed);
        // V5: the token identity is a process-lifetime monotonic nonce bound
        // to the restore generation — never derived from the slot's tail
        // digits, so a completed or restored-away chain can never be
        // re-matched by number reuse (ABA).
        let token = format!("cold-window-e{epoch}-n{serial}");
        *slot = Some(IssuedSearchContinuation {
            token: token.clone(),
            query_key,
            epoch,
            covered_ids: covered,
        });
        let coverage = ContextSearchCoverage {
            complete: false,
            unread_pages: hydration.remaining,
            stop: match hydration.stopped {
                HydrationStop::Complete => ContextSearchCoverageStop::Complete,
                HydrationStop::Budget => ContextSearchCoverageStop::Budget,
                HydrationStop::Deadline => ContextSearchCoverageStop::Deadline,
                HydrationStop::HotCap => ContextSearchCoverageStop::HotCap,
                HydrationStop::Unreadable => ContextSearchCoverageStop::Unreadable,
            },
            continuation: Some(token),
        };
        *self
            .last_search_coverage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = coverage.clone();
        coverage
    }

    /// V4: union two covered-id sequences keeping first-seen order and
    /// dropping duplicates, so the walk's retained set stays bounded by the
    /// distinct pages it actually covered — never by the number of passes.
    fn merge_covered_dedup(
        base: &[ContextItemId],
        extra: Vec<ContextItemId>,
    ) -> Vec<ContextItemId> {
        let mut seen: std::collections::HashSet<ContextItemId> = base.iter().copied().collect();
        let mut merged = Vec::with_capacity(base.len() + extra.len());
        merged.extend_from_slice(base);
        for id in extra {
            if seen.insert(id) {
                merged.push(id);
            }
        }
        merged
    }

    /// S3 + W2 (V5): a continuation token rotates the window it was issued
    /// for — the covered carded hot ids return to the pending directory
    /// (back of the queue, card claims kept), freeing the hot directory so
    /// the drain pages in the next cold region instead of re-serving the
    /// same pages. A token is a valid resume ONLY when it names the live
    /// slot's exact identity, the query binding matches, AND the slot was
    /// issued under the current restore generation. Anything else — stale,
    /// foreign, already-consumed, or pre-restore — is explicitly rejected as
    /// a continuation (`None`): the search runs fresh and its own coverage
    /// stays authoritative, never mixing another walk's covered state.
    async fn rotate_search_window(
        &self,
        query_key: &str,
        token: &str,
    ) -> Option<Vec<ContextItemId>> {
        // The slot is deliberately left in place here: `record_search_coverage`
        // re-issues it with the walk's ACCUMULATED covered set (the serial
        // and the walked pages both live in the slot).
        let issued = self
            .search_continuation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let issued = issued?;
        if issued.token != token || issued.query_key != query_key {
            return None;
        }
        if issued.epoch
            != self
                .continuation_epoch
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return None;
        }
        let mut state = self.state.lock().await;
        let rows = state.external.demote_ids(&issued.covered_ids);
        if !rows.is_empty() {
            state.pending_external_cards.extend(rows);
            state.sync_catalog();
        }
        Some(issued.covered_ids)
    }

    /// W2 (V4/V5) test probe: the live continuation walk's opaque token and
    /// its accumulated covered-id set, exactly as the rotation logic sees
    /// them. Read-only; compiles out outside tests.
    #[cfg(test)]
    pub(crate) fn search_continuation_probe(&self) -> Option<(String, Vec<ContextItemId>)> {
        match self.search_continuation.lock() {
            Ok(slot) => slot
                .as_ref()
                .map(|issued| (issued.token.clone(), issued.covered_ids.clone())),
            Err(poisoned) => poisoned
                .into_inner()
                .as_ref()
                .map(|issued| (issued.token.clone(), issued.covered_ids.clone())),
        }
    }

    /// One card read with the regression gates applied: the deterministic
    /// pause parks the loop exactly at this read boundary, and the failure
    /// bomb turns this read into a transient I/O error. Both compile out
    /// outside tests.
    async fn read_card_with_test_hooks(
        &self,
        dir: &std::path::Path,
        item_id: ContextItemId,
        hash: &str,
        read_index: usize,
    ) -> crate::store::ExternalCardRead {
        #[cfg(test)]
        {
            let pause = self
                .card_read_pause
                .lock()
                .expect("card read pause mutex poisoned")
                .clone();
            if let Some((pause, fire_on)) = pause
                && fire_on == read_index
            {
                pause.planned.notify_one();
                pause.release.notified().await;
            }
            if self
                .card_read_failure_bomb
                .fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |count| if count > 0 { Some(count - 1) } else { None },
                )
                .is_ok()
            {
                return crate::store::ExternalCardRead::IoFailed(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected transient card read failure",
                ));
            }
        }
        let _ = read_index;
        let path = crate::store::external_card_path(dir, item_id, hash);
        crate::store::read_external_card_checked_async(&path, item_id, Some(hash)).await
    }

    /// F2: page in one pending row by id (an id lookup must keep working the
    /// moment a restore returns). Returns whether the entry became live.
    /// N02: the pending row keeps its owner until the read verifies — a
    /// cancelled or dropped fetch leaves the row queued; a transient I/O
    /// failure keeps it retryable; only an installed entry or a verified
    /// absent/damaged card consumes it.
    async fn hydrate_card_for(&self, item_id: ContextItemId) -> bool {
        matches!(
            self.hydrate_card_for_outcome(item_id).await.outcome,
            PendingIdOutcome::Installed
        )
    }

    /// W1 (V2): the per-id card service lane, typed. The same lane
    /// `fetch_external`/`inspect_external`/a directive's own target use —
    /// one card per service, never budget- or cap-blocked, settling its own
    /// residency. The typed outcome lets the required-ref resolution report
    /// *why* a body is unavailable instead of folding every failure into
    /// "absent": a verified absent/structurally-invalid card consumed its
    /// row (a permanent fact), a damaged card names the read failure, a
    /// transient I/O failure keeps the row retryable, and `NoPendingRow`
    /// means the id has no cold owner at all.
    ///
    /// B1: a decided-in-favor read also returns the card's entry, so the
    /// required-ref resolution can plan from the version the row authorizes
    /// even when a later install in the same batch demotes this one back to
    /// pending.
    pub(crate) async fn hydrate_card_for_outcome(&self, item_id: ContextItemId) -> PendingIdRead {
        let hash = {
            let state = self.state.lock().await;
            state
                .pending_external_cards
                .iter()
                .find(|(id, _)| *id == item_id)
                .map(|(_, hash)| hash.clone())
        };
        let Some(hash) = hash else {
            return PendingIdRead::without_entry(PendingIdOutcome::NoPendingRow);
        };
        let dir = crate::store::store_dir(&self.config);
        let outcome = self
            .read_card_with_test_hooks(&dir, item_id, &hash, 0)
            .await;
        let mut state = self.state.lock().await;
        match outcome {
            crate::store::ExternalCardRead::Found(entry)
                if state.external.get(item_id).is_none() =>
            {
                // Same structural check as the batch path (N03): an entry
                // referencing an unknown scope never installs.
                if let Some(scope_id) = entry.scope_id
                    && state.scopes.by_id(scope_id).is_none()
                {
                    state
                        .pending_external_cards
                        .retain(|(id, _)| *id != item_id);
                    state.external_cards_missing = state.external_cards_missing.saturating_add(1);
                    return PendingIdRead::without_entry(PendingIdOutcome::Missing);
                }
                let Some(position) = state
                    .pending_external_cards
                    .iter()
                    .position(|(id, h)| *id == item_id && *h == hash)
                else {
                    return PendingIdRead {
                        outcome: PendingIdOutcome::AlreadyOwned,
                        entry: Some(Box::new(entry)),
                    };
                };
                state.pending_external_cards.remove(position);
                state.external.merge_paged(vec![entry.clone()]);
                state.external.record_card(item_id, hash);
                // S3: per-id installs settle through the one residency
                // entry (protecting the just-served id so the caller's read
                // still finds it). With carded history this slides the hot
                // window: the fetch/inspect path pages history in and the
                // oldest other carded entry back out, so the hot directory
                // stays within the configured caps no matter how many
                // distinct ids are read in sequence.
                settle_metadata_residency(&mut state, &self.config, &[item_id]);
                state.sync_catalog();
                PendingIdRead {
                    outcome: PendingIdOutcome::Installed,
                    entry: Some(Box::new(entry)),
                }
            }
            crate::store::ExternalCardRead::Found(entry) => PendingIdRead {
                outcome: PendingIdOutcome::AlreadyOwned,
                entry: Some(Box::new(entry)),
            },
            crate::store::ExternalCardRead::Missing => {
                state
                    .pending_external_cards
                    .retain(|(id, _)| *id != item_id);
                state.external_cards_missing = state.external_cards_missing.saturating_add(1);
                PendingIdRead::without_entry(PendingIdOutcome::Missing)
            }
            // W1 (V2): a damaged card keeps the same honest accounting the
            // batch/restore paths use (the missing counter), but the typed
            // outcome names the read failure — absence is not proven by an
            // undecodable file.
            crate::store::ExternalCardRead::Corrupt(_) => {
                state
                    .pending_external_cards
                    .retain(|(id, _)| *id != item_id);
                state.external_cards_missing = state.external_cards_missing.saturating_add(1);
                PendingIdRead::without_entry(PendingIdOutcome::Corrupt)
            }
            crate::store::ExternalCardRead::IoFailed(_) => {
                // N02: a transient failure keeps the retryable locator and
                // is counted separately from "the data does not exist".
                state.external_card_io_failures = state.external_card_io_failures.saturating_add(1);
                PendingIdRead::without_entry(PendingIdOutcome::IoFailed)
            }
        }
    }

    /// W1 (V1): probe which scopes the *unread* pending spill cards
    /// reference, without installing anything. The retirement scan's
    /// referenced set only sees loaded owners; a card left unread by the
    /// pass's budgeted drain may still reference a closed scope, and
    /// retiring that scope would consume the card's locator on read-back
    /// (a structurally invalid scope). The probe reads cards under the
    /// same [`HydrationBudget`] shape the drain uses (items + absolute
    /// deadline; the hot caps do not apply — nothing installs):
    /// - a readable card contributes its `scope_id` (a verified reference);
    /// - a missing/damaged card references nothing (its row is dead — the
    ///   drain's missing accounting consumes it independently);
    /// - a transient I/O failure, or rows left when the budget stops, make
    ///   the remainder *unknown*: no retirement proof exists this pass.
    ///
    /// A pure read: no state mutation, no residency change, no double
    /// accounting — the drain keeps sole ownership of row consumption.
    async fn probe_pending_scope_references(&self) -> crate::scope::ScopeRetirementPermit {
        let rows: Vec<(ContextItemId, String)> = {
            let state = self.state.lock().await;
            if state.pending_external_cards.is_empty() {
                return crate::scope::ScopeRetirementPermit::closure_complete();
            }
            state.pending_external_cards.clone()
        };
        let budget = HydrationBudget::for_operation(&self.config);
        let dir = crate::store::store_dir(&self.config);
        let mut permit = crate::scope::ScopeRetirementPermit::closure_complete();
        for (index, (item_id, hash)) in rows.iter().enumerate() {
            let remaining = budget
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            if index >= budget.max_items || remaining.is_zero() {
                permit.pending_unknown = true;
                return permit;
            }
            match tokio::time::timeout(
                remaining,
                self.read_card_with_test_hooks(&dir, *item_id, hash, index),
            )
            .await
            {
                Ok(crate::store::ExternalCardRead::Found(entry)) => {
                    if let Some(scope_id) = entry.scope_id {
                        permit.pending_scope_refs.insert(scope_id);
                    }
                }
                Ok(crate::store::ExternalCardRead::Missing)
                | Ok(crate::store::ExternalCardRead::Corrupt(_)) => {
                    // A dead row references nothing; the drain's own missing
                    // accounting owns consuming it.
                }
                Ok(crate::store::ExternalCardRead::IoFailed(_)) | Err(_) => {
                    // A transient failure or a read that outlived the
                    // remaining deadline: this row's reference is unknown.
                    permit.pending_unknown = true;
                    return permit;
                }
            }
        }
        permit
    }

    /// W1 (V2): resolve the bounded set of required refs against the
    /// pending cold directory *before* required/foreground planning, so
    /// "the body is fetchable by id" and "PromptRequired materializes"
    /// cannot disagree:
    ///
    /// - exact-id (and `context://run/<id>` URI) claims resolve through the
    ///   per-id service lane — one card per claim, never budget- or
    ///   cap-blocked, exactly the established `fetch_external` shape;
    /// - entity/path refs (and current foreground paths) resolve through a
    ///   bounded disk-side scan of the pending rows under the same
    ///   [`HydrationBudget`] items/deadline口径: a card whose entities/path
    ///   match a ref installs through the per-id lane; rows the scan left
    ///   unexamined make absence *unproven* (the typed `UnreadColdPage`
    ///   miss), never a plain `Missing`.
    ///
    /// No whole-history hydration: the claim set is bounded by
    /// `MAX_ANCHOR_ROOT_CLAIMS` (+ the foreground cap) and the scan is
    /// bounded by the operation budget. Nothing runs when the query names
    /// no refs or the pending directory is empty.
    ///
    /// B1: every decided-in-favor per-id read also captures the card's
    /// entry into [`RequiredColdResolution::capture`] — a bounded,
    /// version/range-bound plan source. Residency settlement is unchanged
    /// (each install still slides the hot window); the capture only removes
    /// the planning pass's dependency on who is still resident at the end
    /// of the batch.
    async fn resolve_required_cold_refs(
        &self,
        query: &ContextQuery,
    ) -> crate::materializer::RequiredColdResolution {
        let mut resolution = crate::materializer::RequiredColdResolution::default();
        // Bounded key set, mirroring plan_required's claim filter.
        let claims: Vec<&agent_contracts::AnchorRootClaim> = query
            .hints
            .anchor_roots
            .iter()
            .filter(|claim| claim.strength.requires_prompt())
            .take(agent_contracts::MAX_ANCHOR_ROOT_CLAIMS)
            .collect();
        let foreground_paths: Vec<String> = query
            .hints
            .foreground_resources
            .iter()
            .filter_map(|key| {
                let path = normalize_resource_path(&key.path);
                if path.is_empty()
                    || crate::materializer::foreground_body_already_visible(query, key, &path)
                {
                    None
                } else {
                    Some(path)
                }
            })
            .collect();
        if claims.is_empty() && foreground_paths.is_empty() {
            return resolution;
        }
        let mut exact_ids: Vec<ContextItemId> = Vec::new();
        let mut entity_keys: Vec<String> = Vec::new();
        for claim in &claims {
            match ContextItemId::parse_ref(&claim.item_ref) {
                Ok(id) => exact_ids.push(id),
                Err(_) => entity_keys.push(claim.item_ref.clone()),
            }
        }
        let has_pending = {
            let state = self.state.lock().await;
            !state.pending_external_cards.is_empty()
        };
        if !has_pending {
            return resolution;
        }
        // Exact ids: the per-id lane resolves each target directly (a
        // pending row is an O(1) locator; the outcome types the miss).
        // B1: a decided-in-favor read's entry is captured at read time as a
        // version/range-bound plan source — with the hot cap below the batch
        // size, a later install in this same loop demotes an earlier target
        // back to pending, and planning must not depend on who is still
        // resident when the whole batch ends.
        for id in &exact_ids {
            let read = self.hydrate_card_for_outcome(*id).await;
            if let Some(entry) = read.entry {
                resolution.capture(*id, entry);
            }
            resolution.per_id.insert(*id, read.outcome);
        }
        if entity_keys.is_empty() && foreground_paths.is_empty() {
            return resolution;
        }
        // Entity/path keys: bounded scan of the pending rows. Only a card
        // whose entities/path match a key installs (through the per-id
        // lane); the scan itself never mutates state.
        let rows: Vec<(ContextItemId, String)> = {
            let state = self.state.lock().await;
            state.pending_external_cards.clone()
        };
        if rows.is_empty() {
            return resolution;
        }
        let budget = HydrationBudget::for_operation(&self.config);
        let dir = crate::store::store_dir(&self.config);
        let mut examined = 0usize;
        for (index, (item_id, hash)) in rows.iter().enumerate() {
            let remaining = budget
                .deadline
                .saturating_duration_since(std::time::Instant::now());
            if index >= budget.max_items || remaining.is_zero() {
                break;
            }
            // Only a *decided* read counts as examined: a verified card
            // (match or not) or a verified dead row proves something about
            // this ref; a transient I/O failure or a read cancelled at the
            // deadline proves nothing, so its row stays counted unread —
            // the miss below stays `UnreadColdPage` instead of a false
            // zero-match.
            let decided = match tokio::time::timeout(
                remaining,
                self.read_card_with_test_hooks(&dir, *item_id, hash, index),
            )
            .await
            {
                Ok(crate::store::ExternalCardRead::Found(entry)) => {
                    let entity_hit = entity_keys
                        .iter()
                        .any(|key| entry.entities.iter().any(|entity| entity == key));
                    let path_hit = foreground_paths.iter().any(|path| {
                        entry
                            .entities
                            .iter()
                            .any(|entity| normalize_resource_path(entity) == *path)
                            || entry
                                .file_path
                                .as_deref()
                                .map(normalize_resource_path)
                                .as_deref()
                                == Some(path.as_str())
                    });
                    if entity_hit || path_hit {
                        Some(true)
                    } else {
                        Some(false)
                    }
                }
                Ok(crate::store::ExternalCardRead::Missing)
                | Ok(crate::store::ExternalCardRead::Corrupt(_)) => Some(false),
                Ok(crate::store::ExternalCardRead::IoFailed(_)) | Err(_) => None,
            };
            let Some(matched) = decided else {
                continue;
            };
            examined += 1;
            if matched && !resolution.per_id.contains_key(item_id) {
                // Same B1 capture as the exact-id lane: this install (or a
                // later one in the same scan) can demote an earlier target.
                let read = self.hydrate_card_for_outcome(*item_id).await;
                if let Some(entry) = read.entry {
                    resolution.capture(*item_id, entry);
                }
                resolution.per_id.insert(*item_id, read.outcome);
            }
        }
        resolution.pending_unread = rows.len().saturating_sub(examined);
        resolution
    }
}

/// F2: one capture's spill plan, computed while the state lock is held.
/// Everything expensive is either already accounted for here (bounded
/// serialization) or happens after the lock is released.
#[derive(Debug, Default)]
struct ExternalSpillPlan {
    /// Rows whose card is already on disk for the *current* metadata: they
    /// enter the manifest with no serialization and no write.
    recorded: Vec<(ContextItemId, String)>,
    /// Cards this capture must write: (id, card hash, bytes).
    writes: Vec<(ContextItemId, String, Vec<u8>)>,
    /// Entries this capture serialized. Bounded by the scan budget; an
    /// entry the budgets leave out simply stays inline this time.
    scanned: usize,
}

/// Result of a capture's off-lock card I/O.
#[derive(Debug, Default)]
struct ExternalSpillIo {
    /// Manifest rows: recorded plus cards this capture put on disk.
    spilled: Vec<(ContextItemId, String)>,
    /// Cards proven on disk by this capture (written, or found already
    /// there), to be recorded in the directory under the fresh lock.
    written: Vec<(ContextItemId, String)>,
}

/// Whether an external entry may have its metadata spilled to a card.
/// `Cold` entries still age and count accesses in memory; Pinned and
/// keep-alive entries never leave.
fn spillable_entry(entry: &agent_contracts::ExternalizedContext) -> bool {
    entry.residency == agent_contracts::ContextResidency::External
        && entry.retention != agent_contracts::ContextRetention::Pinned
        && !entry.keep_alive
}

/// F2 phase 1 of a capture: decide what the manifest holds, under budgets
/// that bound the lock-held work rather than only the number of new cards.
fn plan_external_spill(state: &State, config: &SimpleContextConfig) -> ExternalSpillPlan {
    let mut plan = ExternalSpillPlan {
        // Rows a restore has not paged in yet are already on disk and are
        // not in the inline array either: they must stay in the manifest or
        // this capture would drop them.
        recorded: state.pending_external_cards.clone(),
        ..ExternalSpillPlan::default()
    };
    let total = state.external.len();
    if total <= config.external_checkpoint_inline_target {
        return plan;
    }
    let over = total.saturating_sub(config.external_checkpoint_inline_target);
    let mut bytes_left = config.external_checkpoint_card_bytes;
    let mut writes_left = config.external_checkpoint_card_batch;
    // 最旧优先（槽位序即外置序）。
    for entry in state
        .external
        .iter()
        .filter(|entry| spillable_entry(entry))
        .take(over)
    {
        // A recorded card already holds this entry's current metadata: the
        // row is free (one hash lookup, no serialization, no I/O), so it
        // does not spend the scan budget. That is what lets a capture keep
        // a long tail spilled while only paying for *changed* entries.
        if let Some(hash) = state.external.card_hash(entry.item_id) {
            plan.recorded.push((entry.item_id, hash.to_string()));
            continue;
        }
        if plan.scanned >= config.external_checkpoint_scan_budget || writes_left == 0 {
            break;
        }
        plan.scanned += 1;
        let bytes = crate::store::external_card_bytes(entry);
        if bytes.len() as u64 > bytes_left {
            continue;
        }
        bytes_left -= bytes.len() as u64;
        writes_left -= 1;
        let hash = crate::store::checksum_hex(&bytes)[..12].to_string();
        plan.writes.push((entry.item_id, hash, bytes));
    }
    plan
}

fn has_exactly_one_owner(state: &State, item_id: ContextItemId) -> bool {
    // The heap and external map own unique id indexes; the reversible Warm
    // buffer is bounded by config and the externalize-retry list holds the
    // spilled overflow, so checking all four locations is O(1) plus small
    // bounded scans rather than O(total history). The catalog skips a
    // duplicate on rebuild, so it cannot be the duplicate detector.
    let resident = usize::from(state.items.indexes().get(item_id).is_some());
    let warm = usize::from(state.eviction_buffer.iter().any(|item| item.id == item_id));
    let pending = usize::from(
        state
            .pending_externalize_retry
            .iter()
            .any(|item| item.id == item_id),
    );
    let external = usize::from(state.external.get(item_id).is_some());
    resident + warm + pending + external == 1
}

/// Stamp one consumed identity wherever its body/descriptor currently lives.
/// A successful acknowledgement never changes residency or semantic state;
/// it only records that the model actually saw the final packed projection.
fn stamp_consumed(
    state: &mut State,
    item_id: ContextItemId,
    now_tick: u64,
    turn: u64,
    gc_epoch: u64,
) -> bool {
    crate::access::stamp_consumed(state, item_id, now_tick, turn, gc_epoch)
}

#[async_trait::async_trait]
impl ContextEngine for SimpleContextEngine {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        // All lifecycle mutation enters the same lane as multi-phase reads.
        // Most ingress is still one state-lock section; the gate matters when
        // another operation temporarily releases that lock for store I/O, and
        // it also covers the optional distill plan/await/commit span below.
        let _gate = self.op_gate.lock().await;
        // F2: a directive names an item by id and its plan runs under the
        // state lock, so pending spill rows page in first (bounded batches).
        // Plain message/tool ingress never pays for cold metadata.
        //
        // T4: the directive's own target pages in first — per-id service is
        // never budget- or cap-blocked, so the plan always sees its item.
        // The bulk drain stays within the operation budget and may honestly
        // leave rows for the next pass.
        //
        // S3: the ingest path also settles residency through the one entry
        // (protecting the directive's target, whose metadata the plan below
        // must still see) so an ingest cannot leave the hot directory over
        // cap without a typed backpressure fact.
        if matches!(ingress, ContextIngress::ContextDirective { .. }) {
            let mut directive_target: Option<ContextItemId> = None;
            if let ContextIngress::ContextDirective { action } = &ingress {
                let target = directive_item_id(action);
                self.hydrate_card_for(target).await;
                directive_target = Some(target);
            }
            self.hydrate_pending_cards_within_budget(&[]).await;
            let mut state = self.state.lock().await;
            let protect: Vec<ContextItemId> = directive_target.into_iter().collect();
            settle_metadata_residency(&mut state, &self.config, &protect);
        }
        let mut distill: Option<DistillJob> = None;
        // The only lock boundary inside one ingest: a directive may read an
        // externalized blob with the state lock released. `(action, store id
        // when a read is planned, captured ownership checksum)`.
        let mut pending_directive: Option<(ContextAction, Option<ContextItemId>, Option<String>)> =
            None;
        {
            let mut state = self.state.lock().await;
            state.event_seq += 1;

            match ingress {
                ContextIngress::UserMessage { content } => {
                    // A new user message starts a new turn and resets the tool
                    // round counter; the hot entity set is reset to the new
                    // instruction.
                    state.turn += 1;
                    state.tool_round = 0;
                    // Per-turn directive quotas reset at the turn boundary: the
                    // admit/derive caps are per user turn, not per process run.
                    state.admits_this_turn = 0;
                    state.derives_this_turn = 0;
                    state.search_query_stamps_this_turn.clear();
                    // Episode rotation: the working set is bounded by the current
                    // episode plus unresolved semantic state, not by task turns.
                    // A new instruction that is semantically distant from the
                    // current episode (below the token-overlap threshold, and
                    // informative enough to be a phase change rather than a
                    // continuation token), or an episode that exhausted its turn
                    // budget, closes the focus episode: durable outcomes promote
                    // to the task scope, ordinary dialogue leaves the working
                    // set. The transitions are applied here and surfaced by the
                    // next maintenance report.
                    if needs_episode_rotation(&state, &self.config, &content) {
                        // Distill the closing episode: plan under the lock,
                        // compact after it drops. TaskCompleted no longer
                        // uses this operator — it already has an
                        // authoritative CompletionRecord summary. Without a
                        // compactor the rotation is still just
                        // promote-and-evict.
                        if self.compactor.is_some() {
                            distill = plan_episode_distill(&state, &self.config);
                        }
                        let transitions = scope::close_focus_episode(&mut state);
                        state.pending_ingest_transitions.extend(transitions);
                    }
                    state.set_user_hot(entity::extract_entities(&content));
                    if let Some(focus) = state.focus.as_mut() {
                        focus.current_query = content.clone();
                        focus.active_entities = entity::extract_entities(&content);
                        focus.generation += 1;
                    }
                    // A user message with no focus is a session-level message:
                    // the engine never mints a `TaskId` (task identity is
                    // runtime-owned, established via `FocusChanged`), so no
                    // focus is invented here — the item lands in the session
                    // scope and stays selectable while focus is absent.
                    let has_focus = state.focus.is_some();
                    // The user message opens (or touches) the task and focus
                    // scopes of the current work; without a focus this falls
                    // back to the session scope.
                    scope::open_focus_scope(&mut state);

                    let mut item = item::make_item(
                        &state,
                        &self.config,
                        content.clone(),
                        ContextKind::UserMessage,
                        if has_focus {
                            ContextScope::Task
                        } else {
                            ContextScope::Session
                        },
                        ContextRetention::Working,
                        0.62,
                        Some("user".to_string()),
                    );
                    if self.config.supersession && reachability::classify_decision(&content) {
                        // Decisions are promoted and tracked so later decisions
                        // can supersede them.
                        item.tags.push(Label::core(CoreLabel::Decision));
                        item.importance = 0.72;
                    }
                    let item_id = dependency::push_linked(&mut state, &self.config, item);

                    if self.config.supersession && reachability::classify_decision(&content) {
                        let snippet: String = content.chars().take(60).collect();
                        let turn = state.turn;
                        let task_id = state.focus.as_ref().map(|focus| focus.task_id);
                        reachability::queue_decision_supersessions(
                            &mut state,
                            &content,
                            &format!("superseded by decision at turn {turn}: '{snippet}'"),
                            item_id,
                            task_id,
                        );
                    }
                }
                ContextIngress::AssistantMessage { content } => {
                    let item = item::make_item(
                        &state,
                        &self.config,
                        content,
                        ContextKind::AssistantMessage,
                        ContextScope::Task,
                        ContextRetention::Working,
                        0.40,
                        Some("assistant".to_string()),
                    );
                    dependency::push_linked(&mut state, &self.config, item);
                }
                ContextIngress::ToolObservation {
                    output,
                    scope_id,
                    facts,
                } => {
                    state.tool_round += 1;
                    // Typed facts captured on the dispatcher lane are
                    // authoritative; producers without channel-captured
                    // touches fall back to the legacy metadata derivation,
                    // which yields identical values for every producer class
                    // today.
                    let native_facts = output.native_execution_facts();
                    let verify_recipe = facts
                        .as_deref()
                        .or(native_facts.as_ref())
                        .and_then(|facts| facts.verification_probe())
                        .cloned();
                    let facts = facts
                        .as_deref()
                        .filter(|f| !f.resource_touches().is_empty());
                    let (heats, touches) = match facts {
                        Some(facts) => (
                            facts.heats_working_set(output.ok),
                            facts.resource_touches().to_vec(),
                        ),
                        None => (output.heats_working_set(), output.resource_touches()),
                    };
                    let file_path = touches.first().map(|touch| touch.path.clone());
                    let file_revision = touches.first().and_then(|touch| touch.revision.clone());
                    if output.tool_name == "fs.read"
                        && let Some(path) = file_path.as_deref()
                        && !path.is_empty()
                    {
                        let class = classify_fs_read(&state, path);
                        state.record_fs_reread(class);
                    }
                    let file_range = output.file_line_range();
                    let mut content = output.model_content;
                    if let Some(artifact_ref) = output.artifact_ref {
                        content.push_str("\nartifact: ");
                        content.push_str(&artifact_ref);
                    }
                    let ok = output.ok;
                    let round = state.tool_round;
                    let kind = if ok {
                        ContextKind::ToolObservation
                    } else {
                        ContextKind::Error
                    };
                    // Failed observations persist as Working until verified or
                    // superseded; successful observations stay ephemeral and
                    // leave after the model consumes them.
                    let retention = if ok {
                        ContextRetention::Ephemeral
                    } else {
                        ContextRetention::Working
                    };
                    let mut item = item::make_item(
                        &state,
                        &self.config,
                        content.clone(),
                        kind,
                        ContextScope::Turn,
                        retention,
                        if ok { 0.58 } else { 0.82 },
                        Some(format!("tool:{}", output.tool_name)),
                    );
                    // Successful tool identity is the stamped resource,
                    // never tokens mined from stdout. Failed Error items
                    // keep content entities for search and relevance, never proof.
                    if ok {
                        item.entities.clear();
                    }
                    for touch in &touches {
                        entity::index_file_path(&mut item.entities, &touch.path);
                    }
                    if let Some(ref path) = file_path {
                        item.file_path = Some(path.clone());
                    }
                    if let Some(revision) = file_revision {
                        item.file_revision = Some(revision);
                    }
                    if let Some((start, end)) = file_range {
                        // The declared range is the tool-reported interval,
                        // kept as the observation's identity. It is not a
                        // claim of retained coverage: the body above may
                        // already be an engine-clipped partial, and the
                        // supersession proof refuses clipped bodies.
                        item.file_start_line = Some(start);
                        item.file_end_line = Some(end);
                    }
                    // The captured host probe is separate from model-facing
                    // metadata; incomplete legacy associations stay unknown.
                    item.verify_recipe = verify_recipe.clone();
                    // The runtime opened the tool scope at tool start; the
                    // observation is tagged with that frame even though it is
                    // persisted at turn end.
                    if let Some(tool_scope_id) = scope_id {
                        item.scope_id = Some(tool_scope_id);
                    }
                    // The observation itself is the `by` of the intents it
                    // queues: verification (success) or recurrence supersession
                    // (failure). It must exist with its id before queueing so
                    // the semantic state can name it, but it is pushed to the
                    // heap only after queueing so intents never see it.
                    let observation_id = item.id;
                    if self.config.error_verification && !ok {
                        reachability::queue_error_recurrence(&mut state, &item, round);
                    }
                    // Only the same task and host check definition can
                    // verify an earlier fault. Text/entity overlap is not proof.
                    if self.config.error_verification && ok && output.tool_name == "verify.run" {
                        reachability::queue_error_verifications(
                            &mut state,
                            &format!(
                                "error verified fixed by the same task and probe (round {round})"
                            ),
                            observation_id,
                            item.task_id,
                            verify_recipe.as_ref(),
                        );
                    }
                    if ok {
                        // File bodies only (`fs.read`); stamped-path shell
                        // logs must not kill prior observations.
                        reachability::queue_file_body_supersessions(&mut state, &item);
                    }
                    // Structured path stamps may extend tool-hot. Raw
                    // observation entities (stdout) must not.
                    if self.config.entity_affinity && heats {
                        state.merge_tool_hot(
                            touches.into_iter().map(|touch| touch.path),
                            self.config.tool_hot_ttl_turns,
                        );
                    }
                    dependency::push_linked(&mut state, &self.config, item);
                }
                ContextIngress::FocusChanged { mut focus } => {
                    focus.generation += 1;
                    // A new focus replaces user-hot and drops leftover
                    // tool-hot from the previous task.
                    state.tool_hot.clear();
                    state.set_user_hot(focus.active_entities.clone());
                    state.focus = Some(focus);
                    // The focus (and its task) scope opens or reactivates.
                    scope::open_focus_scope(&mut state);
                }
                ContextIngress::FocusCleared => {
                    // Suspend (not complete) the active task: its scopes stay
                    // open so a later FocusChanged with the same task id
                    // resumes them; focus returns to None until then.
                    let task_id = state.focus.as_ref().map(|focus| focus.task_id);
                    state.focus = None;
                    state.tool_hot.clear();
                    state.set_user_hot(Vec::new());
                    if let Some(task_id) = task_id {
                        for scope in state.scopes.iter_mut() {
                            if scope.task_id == Some(task_id) && scope.state == ScopeState::Active {
                                scope.state = ScopeState::Suspended;
                            }
                        }
                    }
                    if let Some(session) = state
                        .scopes
                        .iter()
                        .find(|scope| scope.kind == ScopeKind::Session)
                    {
                        state.active_scope_id = Some(session.id);
                    }
                }
                ContextIngress::WorkingSetSignal {
                    resources,
                    content: _,
                } => {
                    // Successful mid-turn tool commit only. Heat from
                    // stamped resource paths; never from legacy prose.
                    if self.config.entity_affinity {
                        let paths = resources
                            .into_iter()
                            .take(MAX_RESOURCE_TOUCHES)
                            .map(|touch| normalize_resource_path(&touch.path))
                            .filter(|path| !path.is_empty());
                        state.merge_tool_hot(paths, self.config.tool_hot_ttl_turns);
                    }
                }
                ContextIngress::Pin { content, kind } => {
                    // A pin is session-level: it guarantees the session scope
                    // exists even when no task has started yet.
                    scope::ensure_session(&mut state);
                    let item = item::make_item(
                        &state,
                        &self.config,
                        content,
                        kind,
                        ContextScope::Pinned,
                        ContextRetention::Pinned,
                        1.0,
                        Some("explicit-pin".to_string()),
                    );
                    dependency::push_linked(&mut state, &self.config, item);
                }
                ContextIngress::TaskCompleted { task_id, summary } => {
                    // Record which task completed; the scope close (promotion of
                    // durable outcomes, eviction of the working set) happens in
                    // maintain(TaskCompleted) so it is observable.
                    let completed_task =
                        task_id.or_else(|| state.focus.as_ref().map(|f| f.task_id));
                    // The summary belongs to the completed task line, not to
                    // whatever scope happens to be active right now: capture the
                    // completed task's scope *before* the focus/close machinery
                    // runs, so a named summary never inherits the current focus
                    // identity (a completed task can arrive while another task
                    // is focused, and `state.focus` is cleared below before the
                    // item is built).
                    let summary_scope_id = completed_task
                        .and_then(|task| {
                            state.scopes.iter().find(|scope| {
                                scope.kind == ScopeKind::Task
                                    && scope.task_id == Some(task)
                                    && scope.state != ScopeState::Closed
                            })
                        })
                        .map(|scope| scope.id)
                        .or_else(|| {
                            state
                                .scopes
                                .iter()
                                .find(|scope| scope.kind == ScopeKind::Session)
                                .map(|scope| scope.id)
                        });
                    if let Some(completed_task) = completed_task {
                        if state.focus.as_ref().map(|f| f.task_id) == Some(completed_task) {
                            state.focus = None;
                        }
                        // Model hints are per-task: when the task completes its
                        // keep_alive and lease protections expire in *every* body
                        // location, so a completed task cannot keep rooting items
                        // forever. A keep-alive item is normally a GC
                        // root and stays in the heap, but a warm-buffer item from
                        // an older checkpoint must not retain the protection.
                        for item in &mut state.items {
                            if item.task_id == Some(completed_task) {
                                item.keep_alive = false;
                                item.lease_until_turn = None;
                            }
                        }
                        for item in &mut state.eviction_buffer {
                            if item.task_id == Some(completed_task) {
                                item.keep_alive = false;
                                item.lease_until_turn = None;
                            }
                        }
                        // External entries carry the same protection fields
                        // (captured at externalize time); a completed task
                        // clears them there too, so no body location can keep
                        // rooting the finished task's records.
                        let mut external = state.external.take_all();
                        for entry in &mut external {
                            if entry.task_id == Some(completed_task) {
                                entry.keep_alive = false;
                                entry.lease_until_turn = None;
                            }
                        }
                        state.external.replace_all(external);
                        scope::queue_task_scope_close(&mut state, completed_task);
                    }
                    // CompletionRecord.summary is already the validated
                    // bounded outcome. Do not spend a second LLM round
                    // re-summarizing it plus leftover task items.
                    insert_task_summary(
                        &mut state,
                        &self.config,
                        completed_task,
                        summary_scope_id,
                        summary,
                        &[],
                    );
                }
                ContextIngress::ContextDirective { action } => {
                    // Admit of an externalized item reads its content back
                    // from the context store. Plan under this lock, read
                    // outside it, then re-apply under a fresh lock, so the
                    // state lock is never held across disk IO (same phases
                    // as the GC's store step). The apply re-validates the
                    // plan because a concurrent lifecycle transition may
                    // have changed the entry while the file was read.
                    let read_plan = match &action {
                        ContextAction::Admit { item_id, .. } => {
                            match crate::directive::plan_admit(&state, *item_id) {
                                crate::directive::AdmitPlan::Refused(reason) => {
                                    return Err(AgentError::InvalidRequest(reason));
                                }
                                crate::directive::AdmitPlan::ReadExternal(id) => Some(id),
                                crate::directive::AdmitPlan::InMemory
                                | crate::directive::AdmitPlan::Missing => None,
                            }
                        }
                        _ => None,
                    };
                    match read_plan {
                        Some(item_id) => {
                            let expected_checksum = state
                                .external
                                .get(item_id)
                                .and_then(|entry| entry.blob_checksum.clone());
                            pending_directive = Some((action, Some(item_id), expected_checksum));
                        }
                        None => pending_directive = Some((action, None, None)),
                    }
                }
            }
        }

        // Phase 2: the planned external read runs with the state lock
        // released, so a slow store read never blocks unrelated context work.
        #[cfg(test)]
        {
            let pause = self
                .admit_read_pause
                .lock()
                .expect("admit test pause mutex poisoned")
                .clone();
            if let Some(pause) = pause {
                pause.planned.notify_one();
                pause.release.notified().await;
            }
        }
        let external_read = match &pending_directive {
            Some((_, Some(item_id), expected_checksum)) => {
                let dir = crate::store::store_dir(&self.config);
                // The ownership checksum was captured under the lock; the
                // blob must still match it, so a tampered body is a hard
                // read failure, never a silent admit of changed content.
                match crate::store::read_item_checked_async(
                    &dir,
                    *item_id,
                    expected_checksum.as_deref(),
                )
                .await
                {
                    Ok(item) => Some((*item_id, Some(item))),
                    Err(crate::store::StoreReadFailure::Missing) => Some((*item_id, None)),
                    Err(failure) => {
                        return Err(AgentError::Context(format!(
                            "context store read for {item_id} failed: {}",
                            crate::store::read_failure_name(failure)
                        )));
                    }
                }
            }
            _ => None,
        };

        // Phase 3: re-apply the directive under a fresh lock. The admit
        // path re-validates its plan here because a concurrent lifecycle
        // transition may have made the entry terminal while the file read.
        if let Some((action, _, _)) = pending_directive {
            let mut state = self.state.lock().await;
            if let Some(reason) = apply_directive(&mut state, &self.config, action, external_read) {
                // A quota refused the directive: surface it so the model
                // (which believes the hint/lease was granted) learns it
                // was not.
                return Err(AgentError::InvalidRequest(reason));
            }
        }

        if let Some(job) = distill {
            let output = self.run_distill(&job).await;
            // E04/CTX-3: the card states its own coverage — sources the
            // budget left out stay visible as an omission, not a silent
            // shrink.
            let text = if job.excluded_sources > 0 {
                format!(
                    "[episode covers {} of {} sources; earlier sources stay retrievable]
{}",
                    job.source_ids.len(),
                    job.source_ids.len() + job.excluded_sources,
                    output.text
                )
            } else {
                output.text
            };
            let mut state = self.state.lock().await;
            insert_derived_summary(
                &mut state,
                &self.config,
                job.task_id,
                job.summary_scope_id,
                text,
                &job.source_ids,
                job.source_label,
            );
            // COST-7 (R2-11): every compaction call is accounted, whatever
            // its counters — the old "non-zero tokens only" gate made a
            // failed call's Unknown 0/0 row vanish, so a billed-but-failed
            // distill disappeared from the ledger entirely. The identity
            // tells consumers whether zero is a fact or a missing report.
            state.pending_compactions.push(ContextCompaction {
                reason: job.reason,
                input_tokens: output.input_tokens,
                output_tokens: output.output_tokens,
                source_items: job.source_ids.len(),
                usage_identity: output.usage_identity,
                cached_input_tokens: output.cached_input_tokens,
                cache_write_input_tokens: output.cache_write_input_tokens,
                cache_miss_input_tokens: output.cache_miss_input_tokens,
                attempts: output.attempts,
                retries: output.retries,
            });
            state.compaction_input_tokens = state
                .compaction_input_tokens
                .saturating_add(output.input_tokens);
            state.compaction_output_tokens = state
                .compaction_output_tokens
                .saturating_add(output.output_tokens);
        }

        Ok(())
    }

    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        // Maintenance may terminalize, archive, or move a body. Serialize it
        // with plans whose store I/O runs outside the state lock so a stale
        // body cannot be published after the lifecycle transition.
        let _gate = self.op_gate.lock().await;
        let mut state = self.state.lock().await;
        // 维护债务门：无变化时 BeforeModel 是真正的空操作——不扫描、
        // 不消耗序列。生命周期关闭类触发始终执行，它们携带的语义超出
        // 脏工作本身。
        if matches!(trigger, ContextMaintenanceTrigger::BeforeModel)
            && state.last_maintained_seq == state.event_seq
        {
            return Ok(ContextMaintenanceReport::default());
        }
        state.event_seq += 1;
        let now_tick = state.event_seq;
        state.last_maintained_seq = now_tick;
        let turn = state.turn;
        Ok(minor::run_minor(
            &mut state,
            &self.config,
            trigger,
            now_tick,
            turn,
        ))
    }

    async fn gc(&self) -> AgentResult<ContextGcReport> {
        // Serialize against the other multi-phase/whole-state operations:
        // the plan computed below must commit against the same state it was
        // planned against, never one a concurrent restore/storage-GC
        // replaced in the meantime.
        let _gate = self.op_gate.lock().await;
        // Three phases so the state lock is not held across disk IO:
        // 1. plan under the lock (mark/sweep/reactivate/age — in memory);
        // 2. store writes and recall reads without the lock (only
        //    pre-serialized bytes cross into the IO tasks, so a dropped
        //    future abandons writes, never items — they stay in
        //    `pending_externalize_retry` until the commit moves them);
        // 3. commit under a fresh lock (external entries, recalled items,
        //    diagnostics).
        //
        // Recalled blobs are NOT deleted here: this commit is in-memory,
        // not a persistence barrier, and the newest durable checkpoint may
        // still reference the blob. The file stays; the startup reconcile
        // reclaims it as a stale duplicate against the restored state, and
        // Storage GC remains the only other deleter.
        //
        // F2: a pass plans against the whole external set, so pending spill
        // rows page in first (bounded batches, off-lock). B2: the full pass
        // deletes no blobs — recall, reconcile and Storage GC own deletion —
        // so an unread pending row only means its entry is not aged this
        // pass; that is conservative and needs no deferral here.
        //
        // T4: the drain is budget-bounded. Rows left unread keep their owner
        // queued and are simply not aged (and not recallable) this pass; the
        // typed outcome needs no deferral for the memory pass because
        // deletion belongs to Storage GC, which defers on it below.
        //
        // W1 (V1): the drain's completeness now gates scope retirement. An
        // unread pending card's `scope_id` is invisible to the retirement
        // scan, and retiring such a scope would consume the card's locator
        // on read-back. With an incomplete closure the pass probes the
        // unread rows' scope references (bounded, pure-read): a proven
        // reference protects its scope, an unproven remainder defers all
        // retirement this pass (reported as `scope_retirement_deferred`).
        let hydration = self.hydrate_pending_cards_within_budget(&[]).await;
        let retirement_permit = if hydration.complete {
            crate::scope::ScopeRetirementPermit::closure_complete()
        } else {
            self.probe_pending_scope_references().await
        };
        let mut state = self.state.lock().await;
        state.event_seq += 1;
        let now_tick = state.event_seq;
        let turn = state.turn;
        let Some(mut plan) =
            full::plan_full_gc(&mut state, &self.config, now_tick, turn, &retirement_permit)
        else {
            return Ok(ContextGcReport {
                resident: state.items.len(),
                diagnostics: diagnostics::compute(&state),
                ..ContextGcReport::default()
            });
        };
        drop(state);
        let io = full::run_store_io(&self.config, &mut plan).await;
        let mut state = self.state.lock().await;
        let mut report = full::commit_full_gc(&mut state, now_tick, plan, io);
        // T4 second stage (hot/cold bidirectional residency): externalize
        // is the growth path of the hot directory. S3: the commit settles
        // residency through the one entry — entries whose metadata already
        // sits on a spill card return to the pending directory here, so the
        // hot map stays bounded even when history keeps growing; the card
        // claim and the (id, hash) row survive, so per-id fetch and the next
        // budgeted hydration still see them. When nothing demotable remains
        // and the map is still over the caps, the typed backpressure fact
        // lands in the report instead of passing silently.
        let pressure = settle_metadata_residency(&mut state, &self.config, &[]);
        report.hot_metadata_backpressure = !pressure.within_budget();
        Ok(report)
    }

    async fn reconcile_store(&self) -> AgentResult<StoreReconcileReport> {
        // The plain form has no recovery roots to protect: every blob a
        // retained checkpoint references must survive even a startup
        // reconcile, so delegate to the protecting variant with an empty
        // set. Components that know the retained-checkpoint references
        // (the runtime after a restore) call `reconcile_store_protecting`.
        self.reconcile_store_protecting(&[], true).await
    }

    async fn reconcile_store_protecting(
        &self,
        protected: &[ContextItemId],
        roots_complete: bool,
    ) -> AgentResult<StoreReconcileReport> {
        // Same plan/io/commit split as the GC: snapshot the map's owned
        // checksums and the resident ids under the lock, scan + classify
        // the directory without it, then re-own rebuilt blobs under a
        // fresh lock (re-checking that nothing claimed the id meanwhile).
        // The gate keeps this three-phase operation from interleaving with
        // GC/storage-GC/checkpoint/restore, so the re-ownership commit
        // always runs against the state the plan was derived from.
        //
        // `protected` carries the strong recovery roots: item ids whose
        // blobs a still-retained, still-restorable checkpoint references.
        // The deletion branch below refuses to remove those blobs even when
        // the current view already holds the same id resident — a newer
        // snapshot must not silently end the older snapshot's restore
        // promise (R03).
        let _gate = self.op_gate.lock().await;
        // F2: an unpaged spill row is a live owner whose metadata simply is
        // not in memory. Page the rows in before the sweep decides what is
        // orphaned, so paging can never turn into deletion or a re-owned
        // duplicate.
        //
        // B2: a row the drain could not read stays a live owner whose card
        // must survive this pass — pending owner is not ownerless, and root
        // completeness is not metadata completeness. The unread rows fold
        // into the same typed defer as an incomplete root enumeration, so
        // the stale-duplicate and orphan-card sweeps cannot turn a read
        // failure into a deletion.
        //
        // T4: a budget- or cap-stopped drain lands in the same typed defer:
        // the sweep sees `metadata_complete = hydration.complete == false`
        // and defers, while the queued rows stay resumable for the next
        // pass.
        let hydration = self.hydrate_pending_cards_within_budget(&[]).await;
        let (map_checksums, resident_ids) = {
            let mut state = self.state.lock().await;
            state.event_seq += 1;
            let map_checksums: std::collections::HashMap<_, _> = state
                .external
                .iter()
                .map(|entry| (entry.item_id, entry.blob_checksum.clone()))
                .collect();
            let resident_ids: std::collections::HashSet<_> = state
                .items
                .iter()
                .chain(state.eviction_buffer.iter())
                // Retry-list items are still live owners of their id: a
                // blob for such an id is a stale artifact of an abandoned
                // write, not an ownerless orphan to re-own (which would
                // leave the same id owned twice once the retry lands).
                .chain(state.pending_externalize_retry.iter())
                .map(|item| item.id)
                .collect();
            (map_checksums, resident_ids)
        };
        let dir = crate::store::store_dir(&self.config);
        let io = crate::store::run_reconcile_io_protecting(
            &dir,
            &map_checksums,
            &resident_ids,
            protected,
            roots_complete,
            hydration.complete,
        )
        .await;
        let mut state = self.state.lock().await;
        let now_tick = state.event_seq;
        let gc_epoch = state.gc_epoch;
        Ok(crate::store::commit_reconcile(
            &mut state, io, now_tick, gc_epoch,
        ))
    }

    /// Item ids a stored context checkpoint references as external blobs.
    /// Every one is a strong recovery root: while the checkpoint is
    /// retained and restorable, a reconcile must not delete the blob even
    /// if a newer snapshot made the id resident (R03). The runtime unions
    /// these across all retained checkpoints after a restore and passes the
    /// result to `reconcile_store_protecting`.
    async fn checkpoint_recovery_item_ids(
        &self,
        checkpoint: &Value,
    ) -> AgentResult<Vec<ContextItemId>> {
        crate::checkpoint::recovery_item_ids(checkpoint)
    }

    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        // Foreground and required bodies are planned from the current
        // catalog, read without the state lock, then committed as one
        // preview. Serialize that span with GC and whole-state restore so
        // neither operation can replace the stores underneath the plan.
        let _gate = self.op_gate.lock().await;
        // W1 (V2): resolve the bounded set of required/foreground refs
        // against the pending cold directory BEFORE planning. Exact-id
        // claims page their card through the per-id service lane (the
        // `fetch_external` shape); entity/path refs get a bounded cold
        // scan. Without this, the same body could be fetchable by id yet
        // reported Missing to a PromptRequired claim whenever the bulk
        // hydration left its card unread. Disk IO runs off the state lock;
        // the gate serializes the span with GC/restore as everywhere else.
        let resolution = self.resolve_required_cold_refs(&query).await;
        let mut state = self.state.lock().await;
        // A preview is a read: it must not advance the event-sequence clock,
        // so merely materializing never ages TTLs or recency scores.
        state.materialization_revision =
            state
                .materialization_revision
                .checked_add(1)
                .ok_or_else(|| {
                    AgentError::Internal("context materialization id is exhausted".into())
                })?;
        let materialization_id = state.materialization_revision;
        let foreground_plan = materializer::plan_foreground(&state, &query, &[], &resolution);
        let required_plan =
            materializer::plan_required_with_resolution(&state, &query, &resolution);
        drop(state);
        #[cfg(test)]
        {
            let pause = self
                .materialize_io_pause
                .lock()
                .expect("materialize test pause mutex poisoned")
                .clone();
            if let Some(pause) = pause {
                pause.planned.notify_one();
                pause.release.notified().await;
            }
        }
        let dir = crate::store::store_dir(&self.config);
        let (foreground_result, required_result) = tokio::join!(
            materializer::realize_foreground(foreground_plan, &dir),
            materializer::realize_required(required_plan, &dir),
        );
        let (foreground, optional_misses) = foreground_result;
        let (required_bodies, required_misses) = required_result;
        let used: usize = foreground
            .iter()
            .map(|item| item::approx_tokens(&item.content))
            .sum();
        let mut packed_query = query;
        packed_query.budget_tokens = packed_query.budget_tokens.saturating_sub(used);
        for item in &foreground {
            let Some(path) = item
                .file_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
            else {
                continue;
            };
            let path = normalize_resource_path(path);
            let key = match item
                .file_revision
                .as_deref()
                .map(str::trim)
                .filter(|revision| !revision.is_empty())
            {
                Some(revision) => format!("{path}@{revision}"),
                None => path,
            };
            if !packed_query
                .hints
                .checked_files
                .iter()
                .any(|row| row == &key)
            {
                packed_query.hints.checked_files.push(key);
            }
        }
        let mut state = self.state.lock().await;
        let mut materialized = materializer::materialize(&mut state, &self.config, &packed_query);
        materialized.materialization_id = materialization_id;
        materialized.foreground = foreground;
        materialized.optional_misses = optional_misses;
        materializer::apply_required(
            &mut materialized,
            &packed_query,
            required_bodies,
            required_misses,
        );
        materialized.validate_materialization()?;
        state.pending_materialization = Some(PendingMaterialization {
            id: materialization_id,
            item_ids: materialized.items.iter().map(|item| item.item_id).collect(),
            external_item_ids: materialized
                .external
                .iter()
                .map(|entry| entry.item_id)
                .collect(),
            foreground_item_ids: materialized
                .foreground
                .iter()
                .map(|item| item.item_id)
                .collect(),
            foreground_paths: materialized
                .foreground
                .iter()
                .filter_map(|item| {
                    let path = item.file_path.as_deref().map(normalize_resource_path)?;
                    (!path.is_empty()).then_some((item.item_id, path))
                })
                .collect(),
        });
        drop(state);
        Ok(materialized)
    }

    async fn acknowledge_consumption(&self, ack: ContextConsumptionAck) -> AgentResult<()> {
        ack.validate()?;
        let _gate = self.op_gate.lock().await;
        let mut state = self.state.lock().await;
        let pending = state.pending_materialization.as_ref().ok_or_else(|| {
            AgentError::InvalidRequest(
                "context consumption ack has no pending materialization preview".into(),
            )
        })?;
        if pending.id != ack.materialization_id {
            return Err(AgentError::InvalidRequest(format!(
                "context consumption ack references materialization {}, but {} is pending",
                ack.materialization_id, pending.id
            )));
        }
        if ack.item_ids.iter().any(|id| !pending.item_ids.contains(id)) {
            return Err(AgentError::InvalidRequest(
                "context consumption ack contains an item outside the referenced preview".into(),
            ));
        }
        if ack
            .external_item_ids
            .iter()
            .any(|id| !pending.external_item_ids.contains(id))
        {
            return Err(AgentError::InvalidRequest(
                "context consumption ack contains an external ref outside the referenced preview"
                    .into(),
            ));
        }
        if ack
            .foreground_item_ids
            .iter()
            .any(|id| !pending.foreground_item_ids.contains(id))
        {
            return Err(AgentError::InvalidRequest(
                "context consumption ack contains a foreground body outside the referenced preview"
                    .into(),
            ));
        }
        if ack
            .item_ids
            .iter()
            .chain(&ack.external_item_ids)
            .any(|id| !has_exactly_one_owner(&state, *id))
        {
            return Err(AgentError::Context(
                "context consumption ack references an item without exactly one residency owner"
                    .into(),
            ));
        }

        let now_event_seq = state
            .event_seq
            .checked_add(1)
            .ok_or_else(|| AgentError::Internal("context event sequence is exhausted".into()))?;
        // Final-frame exposure, resolved before any mutation so the pending
        // borrow ends: the ack confirms exactly which bodies were rendered.
        let acked_item_ids: std::collections::HashSet<ContextItemId> =
            ack.item_ids.iter().copied().collect();
        let acked_foreground_paths: Vec<String> = pending
            .foreground_paths
            .iter()
            .filter(|(item_id, _)| ack.foreground_item_ids.contains(item_id))
            .map(|(_, path)| path.clone())
            .collect();
        state.event_seq = now_event_seq;
        let turn = state.turn;
        let gc_epoch = state.gc_epoch;
        for item_id in ack.item_ids.iter().chain(&ack.external_item_ids) {
            // Ownership was validated above while holding the same lock, so
            // stamping is infallible and the acknowledgement commits as one
            // mutation rather than partially reinforcing a prefix. The stamp
            // itself must run in release builds too: consumption is the only
            // record that the model actually saw the final packed frame.
            let stamped = stamp_consumed(&mut state, *item_id, now_event_seq, turn, gc_epoch);
            debug_assert!(stamped);
        }
        // Foreground bodies were seen by the model but never changed
        // residency: they are recorded as a weak observational signal only
        // (no access reinforcement, no Admit, no residency transition), so
        // the "model turn N consumed it" invariant covers them without
        // letting transient rehydration reshape the working set.
        state.foreground_consumed_acks = state
            .foreground_consumed_acks
            .saturating_add(ack.foreground_item_ids.len() as u64);
        // The exposure ledger is rewritten from the exact final frame the
        // ack confirms: only bodies that were actually rendered stay
        // classified as previously selected. A body the runtime trimmed out
        // of the final request stops counting as selected, and a
        // foreground-only body the model consumed is attributed as consumed
        // on its next reread instead of reading as unselected/GC-rewarmed.
        let mut final_body_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for item in state.items.iter().chain(state.eviction_buffer.iter()) {
            if acked_item_ids.contains(&item.id)
                && let Some(path) = crate::index::entity::observation_file_path(item)
            {
                let path = normalize_resource_path(path);
                if !path.is_empty() {
                    final_body_paths.insert(path);
                }
            }
        }
        for path in &acked_foreground_paths {
            final_body_paths.insert(path.clone());
        }
        state.selected_body_paths = final_body_paths.clone();
        state
            .selected_descriptor_paths
            .retain(|path| !final_body_paths.contains(path));
        state
            .external_descriptor_paths
            .retain(|path| !final_body_paths.contains(path));
        state.pending_materialization = None;
        Ok(())
    }

    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        let _gate = self.op_gate.lock().await;
        let mut state = self.state.lock().await;
        state.event_seq += 1;
        Ok(scope::open_scope(&mut state, kind, parent))
    }

    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        let _gate = self.op_gate.lock().await;
        let mut state = self.state.lock().await;
        // Only a real close consumes sequence space: a repeated close is a
        // true no-op — `event_seq` counts state changes, not attempts.
        let transitions = scope::close_scope(&mut state, scope_id);
        if !transitions.is_empty() {
            state.event_seq += 1;
        }
        Ok(transitions)
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        let state = self.state.lock().await;
        Ok(diagnostics::compute(&state))
    }

    async fn fs_read_residency(&self, path: &str) -> AgentResult<FsRereadClass> {
        let state = self.state.lock().await;
        Ok(classify_fs_read(&state, path))
    }

    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        // The logical catalog, not just the resident share: the heap, the
        // reversible warm buffer, the externalize-retry list and the external
        // store entries are all known items. External entries project from
        // their descriptor, which carries the authoritative creation clock
        // captured at externalize time.
        //
        // Bounded by construction (F18): `limit == 0` returns before any
        // projection happens, and otherwise the summaries are generated
        // lazily while `bounded_catalog` keeps only the `limit` smallest
        // created_tick rows, so the call's memory stays O(limit) no matter
        // how large the heap or the external store grows — a model-driven
        // catalog call must not cost proportional to logical history size
        // (resource policy). The stream order (heap slot, buffer order,
        // externalization order) makes equal ticks deterministic, exactly
        // like the previous stable sort + truncate.
        if limit == 0 {
            return Ok(Vec::new());
        }
        let state = self.state.lock().await;
        let current_turn = state.turn;
        let mut summaries = crate::heap::bounded_catalog(
            limit,
            state
                .items
                .iter()
                .map(crate::heap::summary_of)
                .chain(state.eviction_buffer.iter().map(crate::heap::summary_of))
                .chain(
                    state
                        .pending_externalize_retry
                        .iter()
                        .map(crate::heap::summary_of),
                )
                .chain(state.external.iter().map(external_summary)),
        );
        // PLATFORM-2: "actually sent" freshness — the body went into the
        // most recent materialized surface of the *current* turn. Turn 0 is
        // the pre-turn baseline, so nothing qualifies there.
        for summary in &mut summaries {
            summary.selected_current_turn =
                current_turn > 0 && summary.last_selected_turn == current_turn;
        }
        Ok(summaries)
    }

    async fn search_external(
        &self,
        query: agent_contracts::ContextSearchQuery,
    ) -> AgentResult<Vec<agent_contracts::ExternalizedContext>> {
        Ok(self
            .search_report_with_continuation(query, None)
            .await?
            .hits)
    }

    async fn search_external_continuation(
        &self,
        query: agent_contracts::ContextSearchQuery,
        continuation: &str,
    ) -> AgentResult<Vec<agent_contracts::ExternalizedContext>> {
        Ok(self
            .search_report_with_continuation(query, Some(continuation))
            .await?
            .hits)
    }

    /// W2 (V3): the serving engine's atomic channel — one pass, one value:
    /// hits, coverage and observation of exactly this search, with no
    /// read-after-call side channel in between.
    async fn search_external_report(
        &self,
        query: agent_contracts::ContextSearchQuery,
        continuation: Option<&str>,
    ) -> AgentResult<agent_contracts::ContextSearchResult> {
        self.search_report_with_continuation(query, continuation)
            .await
    }

    fn last_search_coverage(&self) -> ContextSearchCoverage {
        match self.last_search_coverage.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn last_search_observation(&self) -> ContextSearchObservation {
        match self.search_observation.lock() {
            Ok(slot) => *slot,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    async fn inspect_external(
        &self,
        item_id: ContextItemId,
    ) -> AgentResult<Option<agent_contracts::ExternalizedContext>> {
        // Inspect stamps access state and therefore shares the same mutation
        // lane as GC/materialization even though it performs no store I/O.
        let _gate = self.op_gate.lock().await;
        // F2: an id lookup works the moment a restore returns — a spill row
        // still on the pending directory pages in its own card, without
        // touching the rest of the tail.
        self.hydrate_card_for(item_id).await;
        let mut state = self.state.lock().await;
        state.sync_catalog();
        // 目录级 inspect：Resident/Warm 投影 heap，Stored 用 map 描述符。
        // 终态语义仍被 project_search_hit 过滤，不会把 Tombstoned 交回模型。
        if crate::store::project_search_hit(&state, item_id).is_none() {
            return Ok(None);
        }
        crate::access::stamp_read(&mut state, item_id, agent_contracts::AccessSignal::Inspect);
        Ok(crate::store::project_search_hit(&state, item_id))
    }

    async fn fetch_external(&self, item_id: ContextItemId) -> AgentResult<Option<ContextItem>> {
        // A stored fetch snapshots its owner/checksum, reads without the
        // state lock, then stamps that same owner. Serialize the span with
        // GC and restore so it cannot return bytes from a replaced owner.
        let _gate = self.op_gate.lock().await;
        // F2: a pending spill row pages in by id before the read plan, so a
        // bounded restore never makes a stored body unreachable.
        self.hydrate_card_for(item_id).await;
        // Catalog bodies (Resident / Warm) are returned from heap/buffer.
        // Catalog residency is not the selected working set; this is a
        // stamped read, not a reactivation. Stored bodies still come from
        // disk outside the lock.
        {
            let mut state = self.state.lock().await;
            if let Some(item) = crate::store::catalog_body(&state, item_id) {
                crate::access::stamp_read(
                    &mut state,
                    item_id,
                    agent_contracts::AccessSignal::Fetch,
                );
                return Ok(Some(item));
            }
        }
        let dir = crate::store::store_dir(&self.config);
        let (retrievable, expected_checksum) = {
            let state = self.state.lock().await;
            let entry = state.external.get(item_id);
            (
                entry.is_some_and(crate::store::externally_retrievable),
                entry.and_then(|entry| entry.blob_checksum.clone()),
            )
        };
        if !retrievable {
            return Ok(None);
        }
        let item = match crate::store::read_item_checked_async(
            &dir,
            item_id,
            expected_checksum.as_deref(),
        )
        .await
        {
            Ok(item) => item,
            Err(crate::store::StoreReadFailure::Missing) => return Ok(None),
            Err(failure) => {
                // A checksum mismatch here means the stored body no longer
                // matches the bytes the external entry owns; the entry can
                // never be served again. Surface that as a hard read
                // failure (and the next reconcile quarantines the blob),
                // not as silent content substitution.
                return Err(AgentError::Context(format!(
                    "context store read for {item_id} failed: {}",
                    crate::store::read_failure_name(failure)
                )));
            }
        };
        let mut state = self.state.lock().await;
        // CTX-7: the current entry owns the metadata — merge it over the
        // blob's snapshot so a promoted (or terminal) entry is never
        // overwritten by the stale metadata frozen in the blob.
        let owner = state
            .external
            .get(item_id)
            .filter(|entry| crate::store::externally_retrievable(entry))
            .cloned();
        let Some(owner) = owner else {
            return Ok(None);
        };
        let item = crate::store::reattach_owner_metadata(&owner, item);
        crate::access::stamp_read(&mut state, item_id, agent_contracts::AccessSignal::Fetch);
        Ok(Some(item))
    }

    async fn checkpoint(&self) -> AgentResult<Value> {
        // Serialized with the multi-phase operations so a checkpoint never
        // captures a state torn across a GC/storage-GC commit boundary.
        //
        // CTX-9 残余：外置尾分片——最旧的超额 External 条目把元数据卡片
        // 写进既有 store（内容寻址、幂等），checkpoint 只携带内联段＋
        // `external_spilled` 寻址清单。
        //
        // F2 三阶段（与 GC/storage GC/reconcile 同一形状）：在状态锁内只
        // planning（有界扫描、有界序列化字节），卡片存在性探测与写入在锁
        // 释放后进行，最后取新锁登记卡片并序列化。锁内工作因此与外置历史
        // 长度无关；已有卡片的未变条目连序列化都不做，单次 capture 的成本
        // 跟随*变化*条目数。
        let _gate = self.op_gate.lock().await;
        let plan = {
            let state = self.state.lock().await;
            plan_external_spill(&state, &self.config)
        };
        let io = self.run_external_spill_io(plan).await;
        let mut state = self.state.lock().await;
        for (id, hash) in &io.written {
            state.external.record_card(*id, hash.clone());
        }
        // A manifest row must name an entry this checkpoint really is not
        // carrying inline: either the live map owns it (its metadata is in
        // the card) or it is a still-pending restore row (already on disk).
        // The gate keeps the state from moving under the I/O phase; this is
        // the cheap proof rather than an assumption.
        let pending: std::collections::HashSet<ContextItemId> = state
            .pending_external_cards
            .iter()
            .map(|(id, _)| *id)
            .collect();
        let spilled: Vec<(ContextItemId, String)> = io
            .spilled
            .into_iter()
            .filter(|(id, _)| state.external.get(*id).is_some() || pending.contains(id))
            .collect();
        let mut value = checkpoint::serialize(&state)?;
        if !spilled.is_empty() {
            let spilled_ids: std::collections::HashSet<ContextItemId> =
                spilled.iter().map(|(id, _)| *id).collect();
            let inline: Vec<&agent_contracts::ExternalizedContext> = state
                .external
                .iter()
                .filter(|entry| !spilled_ids.contains(&entry.item_id))
                .collect();
            value["external"] = serde_json::to_value(&inline)
                .map_err(|e| AgentError::Context(format!("checkpoint spill serialize: {e}")))?;
            let spilled_json: Vec<serde_json::Value> = spilled
                .iter()
                .map(|(id, hash)| serde_json::json!({ "id": id.to_string(), "hash": hash }))
                .collect();
            value["external_spilled"] = serde_json::to_value(&spilled_json)
                .map_err(|e| AgentError::Context(format!("checkpoint spill ids: {e}")))?;
        }
        Ok(value)
    }

    async fn restore(&self, data: Value) -> AgentResult<()> {
        // Whole-state replacement must not interleave with a multi-phase
        // plan: a GC plan computed before the restore would otherwise
        // commit stale transitions against the restored state. op_gate
        // serializes this with GC/checkpoint/other restore; the state lock
        // is taken only to install a fully validated candidate.
        let _gate = self.op_gate.lock().await;
        // Deserialize and structurally validate the replacement before it
        // becomes live: a corrupt or hostile checkpoint must not clobber the
        // running state. Present-but-invalid spill rows and duplicate spill
        // ownership are structural contradictions (reject, no mutation),
        // distinct from missing/corrupt cards (honest degrade).
        let spilled = checkpoint::spilled_entries_from_value(&data)?;
        let mut next = checkpoint::deserialize(data)?;
        checkpoint::validate(&next)?;
        checkpoint::reject_duplicate_spill_ownership(&next, &spilled)?;
        // CTX-9 残余：外置尾重水化——分片 checkpoint 只携带内联段，卡片
        // 由这里从既有 store 读回。op_gate 已把整个 restore 与 GC/其他
        // restore 串行化：卡片 IO 不持 state 锁，不会交错任何结构变更。
        // 卡片缺失/损坏（典型：已过保留窗的旧 checkpoint，其条目随后被
        // Storage GC 删除）→ 该条目缺席＋计数如实上报，恢复整体不失败
        // ——与旧 checkpoint 的 blob 缺失同一诚实降级，绝不伪造完整。
        //
        // F2: the read is *bounded* — at most `external_restore_card_batch`
        // cards are paged in before restore returns. The rest stay as
        // `(id, card hash)` rows in `pending_external_cards`: the ids are
        // known (so no blob or card can be reclaimed under them and an id
        // lookup pages its own card in) while the metadata is not in
        // memory, so a restore's cost does not track total history length.
        // Merge and re-validate stay on the local candidate (F1): a
        // structural reject never installs, and the live lock is taken only
        // after the candidate is valid.
        let mut missing_cards: u64 = 0;
        let mut io_failed_cards: u64 = 0;
        let mut rehydrated: Vec<(agent_contracts::ExternalizedContext, String)> = Vec::new();
        let mut deferred_cards: Vec<(ContextItemId, String)> = Vec::new();
        if !spilled.is_empty() {
            let dir = crate::store::store_dir(&self.config);
            let batch = self.config.external_restore_card_batch;
            for (index, (id, hash)) in spilled.iter().enumerate() {
                if index >= batch {
                    deferred_cards.push((*id, hash.clone()));
                    continue;
                }
                let path = crate::store::external_card_path(&dir, *id, hash);
                match crate::store::read_external_card_checked_async(&path, *id, Some(hash)).await {
                    crate::store::ExternalCardRead::Found(entry) => {
                        rehydrated.push((entry, hash.clone()));
                    }
                    // N02: a transient I/O failure never consumes the
                    // locator — the row stays queued (the manifest keeps
                    // recording it), and the next bounded drain retries it.
                    crate::store::ExternalCardRead::IoFailed(_) => {
                        deferred_cards.push((*id, hash.clone()));
                        io_failed_cards += 1;
                    }
                    // 缺失、损坏与哈希失配同一诚实计数（数据不存在的
                    // 永久事实，恢复如实降级）。
                    crate::store::ExternalCardRead::Missing
                    | crate::store::ExternalCardRead::Corrupt(_) => missing_cards += 1,
                }
            }
        }
        if !rehydrated.is_empty() || missing_cards > 0 {
            let mut entries: Vec<agent_contracts::ExternalizedContext> = next.external.take_all();
            entries.extend(rehydrated.iter().map(|(entry, _)| entry.clone()));
            entries.sort_by_key(|entry| entry.externalized_at_tick);
            next.external.replace_all(entries);
            next.external_cards_missing = next.external_cards_missing.saturating_add(missing_cards);
        }
        next.external_card_io_failures = next
            .external_card_io_failures
            .saturating_add(io_failed_cards);
        // The card each rehydrated entry came from still describes it, so the
        // next capture re-uses that file instead of serializing the entry
        // again (`replace_all` cleared the directory, hence after it).
        for (entry, hash) in &rehydrated {
            next.external.record_card(entry.item_id, hash.clone());
        }
        next.pending_external_cards = deferred_cards;
        if !spilled.is_empty() {
            next.sync_catalog();
            checkpoint::validate(&next)?;
        }
        let mut state = self.state.lock().await;
        // The revision is a process-lifetime nonce, not rollbackable task
        // state. Keeping the larger live value prevents restore from reusing
        // a preview id and accepting a delayed pre-restore acknowledgement
        // against a newly materialized frame (ABA).
        next.materialization_revision = next
            .materialization_revision
            .max(state.materialization_revision);
        *state = next;
        state.sync_catalog();
        crate::reactivation::clear_segment(&mut state);
        // Old checkpoints predate the entity signature cache; backfill it
        // once so restored items keep scoring and dependency behavior. The
        // heap method re-indexes each backfilled signature, so the entity
        // index stays consistent without a wholesale rebuild.
        let backfills: Vec<(usize, Vec<String>)> = state
            .items
            .items_mut()
            .iter_mut()
            .enumerate()
            .filter(|(_, item)| {
                item.entities.is_empty() && !crate::item::is_raw_evidence_kind(item.kind)
            })
            .map(|(index, item)| (index, entity::extract_entities(&item.content)))
            .collect();
        for (index, entities) in backfills {
            state.items.update_entities(index, entities);
        }
        // Pre-split checkpoints expressed semantic death as lifecycle
        // labels; migrate them to the SemanticState dimension so GC and
        // the materializer treat restored items like live ones. Semantic
        // state is not indexed, so the raw mutable slice is safe here.
        for item in state.items.items_mut() {
            if item.semantic.is_live() {
                if item
                    .tags
                    .iter()
                    .any(|tag| tag.is_lifecycle(agent_contracts::LifecycleLabel::Superseded))
                {
                    item.semantic = agent_contracts::SemanticState::Superseded { by: None };
                } else if item
                    .tags
                    .iter()
                    .any(|tag| tag.is_lifecycle(agent_contracts::LifecycleLabel::VerifiedFixed))
                {
                    item.semantic = agent_contracts::SemanticState::VerifiedFixed { by: None };
                }
            }
        }
        // Restore can rewrite entity signatures and semantic death; rebuild
        // the derived catalog before the next search rather than trusting
        // the pre-migration directory.
        state.catalog_dirty.mark_rebuild();
        // W2 (V5): the installed state replaced the catalog the continuation
        // walk was cursored over, so every outstanding token is now a claim
        // about a view that no longer exists. Invalidate the live walk and
        // bump the restore generation — a pre-restore token arriving later is
        // explicitly rejected (it degrades to a fresh search) instead of
        // rotating a window over content this view never searched. A
        // REJECTED restore never reaches this point: its failure paths
        // return before the state is installed, leaving the live chain
        // untouched.
        self.continuation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *self
            .search_continuation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        Ok(())
    }

    async fn storage_gc(&self) -> AgentResult<agent_contracts::StorageGcReport> {
        self.storage_gc_protecting(&[], true).await
    }

    /// W03: the deletion entry honors the same retained-root invariant as
    /// reconcile — protected recovery roots join the strong-reference set,
    /// and an incomplete root enumeration defers all deletion.
    async fn storage_gc_protecting(
        &self,
        protected_recovery_roots: &[agent_contracts::ContextItemId],
        roots_complete: bool,
    ) -> AgentResult<agent_contracts::StorageGcReport> {
        // Plan under the lock, delete outside it, commit under a fresh
        // lock — the state lock is never held across disk IO. The gate
        // serializes this with GC/reconcile/checkpoint/restore so the
        // commit always sees the state the plan was derived from.
        let _gate = self.op_gate.lock().await;
        // F2: deletion must see every owner. A pending spill row's metadata
        // is not in memory, so it pages in before the plan — paging is never
        // allowed to look like an unreferenced blob.
        //
        // B2: a row the drain could not read (transient I/O) is still a live
        // owner whose dependency edges are invisible to the planner — adding
        // the pending *id* to the roots would not reveal its edges. Pending
        // owner is not ownerless, and root completeness is not metadata
        // completeness: with unread cold metadata the deletion branch defers
        // exactly like an incomplete root enumeration.
        //
        // T4: a budget- or hot-cap-stopped drain lands in the same typed
        // defer, and the report names the resumable remainder instead of a
        // bare "incomplete".
        let hydration = self.hydrate_pending_cards_within_budget(&[]).await;
        let plan = {
            let mut state = self.state.lock().await;
            state.event_seq += 1;
            let now_tick = state.event_seq;
            store::plan_storage_gc(
                &state,
                &self.config,
                now_tick,
                protected_recovery_roots,
                roots_complete,
                hydration,
            )
        };
        let dir = store::store_dir(&self.config);
        let io = store::run_storage_io(&dir, &plan).await;
        let mut state = self.state.lock().await;
        Ok(store::commit_storage_gc(&mut state, plan, io))
    }
}

/// Apply one model/operator context directive to the current state. Every
/// directive targets an existing item — in the heap or the reversible
/// eviction buffer (a hint/lease on an evicted item is what brings it
/// back on the next GC pass); a stale `item_id` (already externalized or
/// superseded) is a silent no-op. GC reads the resulting fields, so every
/// "kept because ..." is explainable in the eviction reasons.
///
/// Returns `Some(reason)` when the directive was refused by a quota:
/// `keep_alive` and leases are bounded so the model cannot root the whole
/// heap. A refused directive leaves the item unchanged — the caller
/// surfaces the reason to the model.
/// Whether the next user message should rotate the focus episode. Two
/// signals, both explainable:
/// - a semantic boundary: the new instruction shares almost no tokens with
///   the current episode's query AND carries real information (entities or
///   enough length) — a bare continuation token ("continue", "ok", "next")
///   does not rotate;
/// - the turn budget: even perfectly related instructions rotate once the
///   episode exceeded `episode_max_user_turns` user turns.
fn needs_episode_rotation(state: &State, config: &SimpleContextConfig, content: &str) -> bool {
    let Some(focus) = state.focus.as_ref() else {
        return false;
    };
    if focus.generation >= config.episode_max_user_turns as u64 {
        return true;
    }
    let overlap = crate::policy::lexical_overlap(content, &focus.current_query);
    // Continuation tokens carry no entities and are short; only a message
    // with real entities or a genuinely long body can signal a phase
    // change. The overlap check already covers entity-sharing continuations
    // ("keep fixing AuthService.rs" shares the entity, so overlap is high).
    let informative =
        !entity::extract_entities(content).is_empty() || content.chars().count() >= 12;
    overlap < config.episode_rotate_threshold && informative
}

fn apply_directive(
    state: &mut State,
    config: &SimpleContextConfig,
    action: ContextAction,
    external_read: Option<(ContextItemId, Option<ContextItem>)>,
) -> Option<String> {
    let target_id = directive_item_id(&action);

    // Admit and Derive have their own quota checks and mutation logic (the
    // admit path may need the store read planned by `ingest`); dispatch
    // them before the in-memory directive machinery.
    match &action {
        ContextAction::Admit { item_id, reason } => {
            return crate::directive::apply_admit(state, config, *item_id, reason, external_read);
        }
        ContextAction::Derive { item_id, fact, .. } => {
            return crate::directive::apply_derive(state, config, *item_id, fact.clone());
        }
        // The anchor-root projection is a bounded whole-set replacement:
        // task authority stays with the TaskManager, so there is no per-claim
        // mutation to serialize with — the engine only mirrors the current
        // projection for its GC and materialization passes.
        ContextAction::AnchorRoots { roots } => {
            if roots.len() > agent_contracts::MAX_ANCHOR_ROOT_CLAIMS {
                return Some(format!(
                    "anchor roots refused: {} claims exceed the cap of {}",
                    roots.len(),
                    agent_contracts::MAX_ANCHOR_ROOT_CLAIMS
                ));
            }
            state.anchor_roots = roots.clone();
            return None;
        }
        ContextAction::CheckedFiles { files } => {
            let mut files = files.clone();
            if files.len() > agent_contracts::MAX_CHECKED_FILE_HINTS {
                let drop = files.len() - agent_contracts::MAX_CHECKED_FILE_HINTS;
                files.drain(0..drop);
            }
            state.checked_files = files;
            return None;
        }
        _ => {}
    }

    // Quota checks run on read-only views first; the mutation happens after,
    // so the checks never contend with the mutable borrow.
    let refusal = match &action {
        // `keep=false` always applies (releasing cannot exceed a cap);
        // `keep=true` is bounded by the keep-alive quota.
        ContextAction::GcHint {
            keep_alive: true, ..
        } => {
            // Quotas are global across body locations: a keep_alive item in
            // the warm buffer still consumes the cap — and so does one in
            // the externalize-retry list (CTX-6: it owns a full body).
            let kept = state
                .items
                .iter()
                .chain(&state.eviction_buffer)
                .chain(&state.pending_externalize_retry)
                .filter(|item| item.keep_alive)
                .count();
            (kept >= config.max_keep_alive_items).then(|| {
                format!(
                    "gc_hint refused: {kept} items are already keep_alive (cap {})",
                    config.max_keep_alive_items
                )
            })
        }
        ContextAction::Lease { .. } => {
            // CTX-6: a lease target must resolve wherever its body lives,
            // including the externalize-retry list — otherwise the directive
            // silently no-ops on a body the state owns.
            let target = state
                .items
                .iter()
                .find(|item| item.id == target_id)
                .or_else(|| {
                    state
                        .eviction_buffer
                        .iter()
                        .find(|item| item.id == target_id)
                })
                .or_else(|| {
                    state
                        .pending_externalize_retry
                        .iter()
                        .find(|item| item.id == target_id)
                });
            match target {
                // Stale target: silent no-op, same as the mutation path.
                None => None,
                Some(item) => {
                    // A lease is bounded per directive and per task: the
                    // model cannot lease an item forever, nor lease a task's
                    // whole history into roots.
                    let task = item.task_id;
                    // Renewing an item that is already leased adds no new
                    // leased item or tokens, so it never trips the quota.
                    let already_leased = item
                        .lease_until_turn
                        .is_some_and(|until| until >= state.turn);
                    // Leased-item accounting is global across body locations:
                    // a leased item in the warm buffer still counts against
                    // the task cap, and so does one in the retry list.
                    let (leased, leased_tokens) = state
                        .items
                        .iter()
                        .chain(&state.eviction_buffer)
                        .chain(&state.pending_externalize_retry)
                        .filter(|other| {
                            other
                                .lease_until_turn
                                .is_some_and(|until| until >= state.turn)
                                && other.task_id == task
                        })
                        .fold((0usize, 0usize), |(count, tokens), other| {
                            (
                                count + 1,
                                tokens + crate::item::approx_tokens(&other.content),
                            )
                        });
                    let added = usize::from(!already_leased);
                    let added_tokens = if already_leased {
                        0
                    } else {
                        crate::item::approx_tokens(&item.content)
                    };
                    if leased.saturating_add(added) > config.max_leased_items_per_task {
                        Some(format!(
                            "lease refused: task would lease {} items (cap {})",
                            leased.saturating_add(added),
                            config.max_leased_items_per_task
                        ))
                    } else if leased_tokens.saturating_add(added_tokens)
                        > config.max_leased_tokens_per_task
                    {
                        Some(format!(
                            "lease refused: task would lease {} tokens (cap {})",
                            leased_tokens.saturating_add(added_tokens),
                            config.max_leased_tokens_per_task
                        ))
                    } else {
                        None
                    }
                }
            }
        }
        _ => None,
    };
    if let Some(reason) = refusal {
        return Some(reason);
    }

    let mut tagged = false;
    {
        // CTX-6: in-place directives (gc_hint/tag/lease) reach a body in the
        // externalize-retry list too — a control action must actually
        // execute against every in-memory owner, never silently no-op.
        let mut target = state
            .items
            .iter_mut()
            .chain(state.eviction_buffer.iter_mut())
            .chain(state.pending_externalize_retry.iter_mut())
            .find(|item| item.id == target_id);
        if let Some(item) = target.as_mut() {
            match action {
                ContextAction::GcHint { keep_alive, .. } => {
                    item.keep_alive = keep_alive;
                }
                ContextAction::Tag { tag, .. } => {
                    let label = Label::extension(tag);
                    if !item.tags.contains(&label) {
                        item.tags.push(label);
                        tagged = true;
                    }
                }
                ContextAction::Lease { turns, .. } => {
                    let turns = turns.min(config.max_lease_turns);
                    item.lease_until_turn = Some(state.turn.saturating_add(turns as u64));
                }
                // The runtime owns the GC pass; `context.collect` never arrives
                // as an ingest directive (the actor calls `ContextEngine::gc`).
                ContextAction::Collect => {}
                // Admit/Derive/AnchorRoots/CheckedFiles are dispatched above
                // and never reach the in-memory directive loop.
                ContextAction::Admit { .. }
                | ContextAction::Derive { .. }
                | ContextAction::AnchorRoots { .. }
                | ContextAction::CheckedFiles { .. } => unreachable!(),
            }
        }
    }
    if tagged {
        state.mark_catalog(target_id);
    }
    None
}

fn directive_item_id(action: &ContextAction) -> ContextItemId {
    match action {
        ContextAction::GcHint { item_id, .. }
        | ContextAction::Tag { item_id, .. }
        | ContextAction::Lease { item_id, .. }
        | ContextAction::Admit { item_id, .. }
        | ContextAction::Derive { item_id, .. } => *item_id,
        ContextAction::Collect
        | ContextAction::AnchorRoots { .. }
        | ContextAction::CheckedFiles { .. } => ContextItemId::new(),
    }
}

#[cfg(test)]
mod freeze_pin {
    use super::SimpleContextConfig;

    #[test]
    fn gc_thresholds_are_freeze_pinned() {
        let config = SimpleContextConfig::default();
        assert!(
            (config.active_threshold - 0.58).abs() < f32::EPSILON,
            "do not retune active_threshold (item 21); got {}",
            config.active_threshold
        );
        assert!(
            (config.archive_threshold - 0.24).abs() < f32::EPSILON,
            "do not retune archive_threshold (item 21); got {}",
            config.archive_threshold
        );
        assert_eq!(
            config.gc_max_generation, 3,
            "do not retune gc_max_generation (item 21)"
        );
    }
}
