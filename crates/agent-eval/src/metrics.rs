//! All-module cost accounting for M15: aggregate every measurable dimension
//! of a run from its event stream without calling a model.
//!
//! The M15 evaluation plan requires per-solved-task accounting of model
//! tokens, tool-schema tokens, lifecycle/GC cost and tool behavior. Some of
//! that is only measurable in the live harness (wall time, provider
//! latency, process launches); everything that is *in the event stream* is
//! aggregated here deterministically, so the accounting logic is tested
//! without a model and reused by the live harness.

use agent_contracts::{
    CAPABILITY_MANAGE, CONTEXT_MANAGE, ContextItemId, FS_READ_MOTIVE_KEY, FsReadMotive,
    RuntimeEvent, RuntimeEventEnvelope,
};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecoverPath {
    Search,
    Reactivate,
}

/// Deterministic per-run measurements aggregated from the event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunMetrics {
    // Cost.
    /// Provider-reported input tokens (`ModelUsed`).
    pub model_input_tokens: u64,
    /// Provider-reported output tokens (`ModelUsed`).
    pub model_output_tokens: u64,
    /// Transport attempts that produced a `ModelUsed` (successful round).
    pub model_attempts: u64,
    /// Retries inside those successful rounds (`attempts - 1` per event).
    pub model_retries: u64,
    /// True when any successful round retried. Recorded tokens omit failed
    /// attempts that reported no usage, so they are a lower bound.
    pub provider_tokens_lower_bound: bool,
    /// Cumulative tool-schema tokens of every round surface
    /// (`ToolSurfacePlanned.selected_schema_tokens`).
    pub schema_tokens_total: u64,
    /// Model rounds that produced a surface plan.
    pub rounds: u64,
    /// User turns (`UserMessageAccepted` with Applied lifecycle).
    pub turns: u64,
    /// Tool calls (`ToolStarted`).
    pub tool_calls: u64,
    /// Lifecycle transitions produced by maintenance passes
    /// (`ContextMaintained.report.transitions`).
    pub lifecycle_transitions: u64,
    /// Full-GC evictions / reactivations / externalizations
    /// (`ContextGc`).
    pub gc_evictions: u64,
    pub gc_reactivations: u64,
    pub gc_externalizations: u64,

    // Tool behavior.
    /// Tool results with `ok: false`.
    pub failed_tool_outputs: u64,
    /// Tool results carrying an artifact reference (spill happened).
    pub artifact_spills: u64,
    /// Cumulative model-facing output chars of tool results.
    pub output_chars_total: u64,
    /// Repeated `fs.read` of the same workspace path (the second and later
    /// reads of one path — a proxy for search/re-read inefficiency).
    pub repeated_fs_reads: u64,
    /// Engine-classified `fs.read` attribution (last diagnostics snapshot).
    pub reread_previously_selected: u64,
    pub reread_selected_descriptor: u64,
    pub reread_external_descriptor: u64,
    pub reread_resident_unselected: u64,
    pub reread_warm: u64,
    pub reread_stored: u64,
    pub reread_first_read: u64,
    /// E2E `fs.read` motive from Runtime-stamped ToolFinished metadata.
    /// Use these to answer "did GC add rounds?" (`warm`/`stored`) vs
    /// identity-known duplicates (`checked-fresh`) vs "prompt only had
    /// `path@rev`" (`descriptor-only`) vs trajectory (`body-visible-current`).
    pub reread_motive_first: u64,
    pub reread_motive_body_visible_current: u64,
    pub reread_motive_descriptor_only: u64,
    /// 正文此前被消费、摘要未变、当前帧只剩身份——协议层正文缓存
    /// 可服务的群体。
    pub reread_motive_protocol_checkpoint_body_missing: u64,
    pub reread_motive_checked_fresh: u64,
    pub reread_motive_needs_revalidation: u64,
    pub reread_motive_warm: u64,
    pub reread_motive_stored: u64,
    pub reread_motive_changed: u64,
    /// Selected-token attribution across `ContextPrepared` previews.
    pub selected_tokens_by_kind: BTreeMap<String, u64>,
    pub selected_tokens_by_reason: BTreeMap<String, u64>,
    pub selected_tokens_by_source: BTreeMap<String, u64>,
    pub selected_tokens_reactivated: u64,
    pub selected_tokens_resident: u64,
    /// Counts of trusted `metadata.failure_class` values (`TOOL-ERROR-01`).
    /// Includes classified no-match searches that still have `ok: true`.
    pub tool_failure_classes: BTreeMap<String, u64>,
    /// Structured edit attempts observed at `ToolFinished`. This includes
    /// runtime refusals that deliberately never reached `ToolStarted`.
    pub edit_attempts: u64,
    /// Structured edit calls that reached execution (`ToolStarted`).
    pub edit_started_calls: u64,
    pub edit_successes: u64,
    /// Successful outputs that actually proposed and durably committed a
    /// changed body (`metadata.changed=true`), excluding successful no-ops.
    pub edit_committed_changes: u64,
    pub edit_failures: u64,
    /// A per-cell denominator/numerator for the first structured edit
    /// attempt. This is intentionally not called "valid": only a
    /// fixture-level oracle can prove that the model supplied valid args.
    pub edit_first_attempts: u64,
    pub edit_first_attempt_successes: u64,
    pub edit_first_attempt_committed_changes: u64,
    /// Started edits with no matching terminal output in the captured
    /// trace (usually cancellation or an incomplete trace).
    pub edit_unfinished_calls: u64,
    /// Paired ToolStarted -> ToolFinished latency for structured edits.
    pub edit_ms_p50: u64,
    pub edit_ms_p95: u64,
    /// Time from the first structured edit attempt to the final captured
    /// event. A passing hidden verifier may use this as an explicitly
    /// labelled edit-to-final-verification proxy; it is not an intermediate
    /// green measurement.
    pub edit_to_trace_end_ms: u64,
    /// Successful `fs.read` bytes reported by the tool.
    pub fs_read_bytes_total: u64,
    /// Bytes before/after successful structured edits. Failed or unsettled
    /// effects intentionally do not expose proposed bodies, so these remain
    /// successful-edit totals rather than estimates.
    pub edit_success_bytes_before: u64,
    pub edit_success_bytes_after: u64,
    /// `fs.read` calls on a path after a successful structured edit to that
    /// path. The edit result already carries a revision and bounded echo,
    /// so this exposes avoidable confirmation reads.
    pub post_edit_confirm_reads: u64,
    /// First shell/full-write fallback after an edit-failure episode. Shell
    /// is only a sequence proxy: events do not claim that the command edited
    /// a file.
    pub edit_failure_shell_fallback_proxy: u64,
    pub edit_failure_fs_write_fallback: u64,
    /// Runtime settlement outcomes for structured edits.
    pub edit_commit_not_applied: u64,
    pub edit_commit_recovery_required: u64,
    pub edit_commit_unknown: u64,

    // Materialization (the Phase 0 measurement baseline: how big the model
    // input actually was and where the working set lived).
    /// `ContextPrepared` events: model rounds that produced a
    /// materialization preview.
    pub materialize_rounds: u64,
    /// Cumulative selected items across previews (`selected.len()`).
    pub selected_items_total: u64,
    /// Cumulative selected-item token estimate (`ContextSelection.approx_tokens`).
    pub selected_tokens_total: u64,
    /// Cumulative `approx_active_tokens` across previews — the engine's
    /// estimate of the model-visible working set per round.
    pub active_tokens_total: u64,
    /// Final materialization diagnostics: the residency split the last
    /// preview saw (resident heap / warm buffer / cold store / external).
    pub final_total_items: u64,
    pub final_resident_items: u64,
    pub final_warm_items: u64,
    pub final_cold_items: u64,
    pub final_external_items: u64,
    /// Last preview's Resident heap body bytes (`ContextDiagnostics.resident_bytes`).
    pub final_resident_bytes: u64,
    /// Max Resident heap body bytes seen across previews.
    pub peak_resident_bytes: u64,
    /// Cumulative store I/O from full-GC passes (`ContextGc`): bytes
    /// written (externalization), bytes read back (recall), and items
    /// recalled from the store.
    pub store_write_bytes_total: u64,
    pub store_read_bytes_total: u64,
    pub store_recalled_items_total: u64,
    /// Materialization latency percentiles (ms) across `ContextPrepared`
    /// previews (the engine's own materialize call, before runtime
    /// rendering overhead). 0 when no preview carried a timestamp.
    pub materialize_ms_p50: u64,
    pub materialize_ms_p95: u64,

    // Retrieval（M15 / CTX-DISC）：从事件流计 search/inspect/fetch，
    // 并用 GC 外置/驱逐 id 与后续命中做 found-after-forgotten 连接。
    /// `context.manage` / `capability.manage` search calls.
    pub search_calls: u64,
    /// Descriptor rows returned by those searches.
    pub search_hits: u64,
    /// Searches that returned zero descriptors.
    pub search_empty: u64,
    pub search_miss_not_found: u64,
    pub search_miss_evidence_absent: u64,
    pub search_miss_provider_unavailable: u64,
    /// Search latency from `ToolStarted`→`ToolFinished` envelope timestamps.
    pub search_ms_p50: u64,
    pub search_ms_p95: u64,
    pub inspect_calls: u64,
    pub fetch_calls: u64,
    pub admit_calls: u64,
    /// Unique ids evicted or externalized this run.
    pub forgotten_items: u64,
    /// Forgotten ids later seen in a search/inspect/fetch/admit/reactivation.
    pub recovered_items: u64,
    /// Forgotten ids whose *first* recovery was an explicit search/inspect/fetch/admit.
    pub recovery_explicit_search: u64,
    /// Forgotten ids whose *first* recovery was a GC reactivation.
    pub recovery_auto_reactivation: u64,
    pub reactivation_selected: u64,
    pub reactivation_consumed: u64,
    pub reactivation_selected_tokens: u64,
    pub reactivation_consumed_tokens: u64,
    pub reactivation_events: u64,
    pub unique_reactivated: u64,
    pub reactivated_tokens: u64,
    pub reactivation_tool_observation_selected: u64,
    pub reactivation_tool_observation_consumed: u64,
    pub reactivation_file_observation_selected: u64,
    pub reactivation_file_observation_consumed: u64,
    pub prompt_system_tokens: u64,
    pub prompt_runtime_facts_tokens: u64,
    pub prompt_task_anchor_tokens: u64,
    pub prompt_task_progress_tokens: u64,
    pub prompt_current_focus_tokens: u64,
    pub prompt_historical_context_tokens: u64,
    pub prompt_turn_frame_tokens: u64,
    pub prompt_tool_schema_tokens: u64,
    /// Sum of `PromptLayerCosts.tool_catalog_index_tokens` across rounds.
    pub prompt_tool_catalog_index_tokens: u64,
    /// Repeated `fs.read` of the same path (workspace reread, not an id partition).
    pub recovery_workspace_reread: u64,
    /// Forgotten ids that were never recovered.
    pub recovery_failed: u64,
    /// Final diagnostics snapshot of graded access stamps.
    pub access_search_hits: u64,
    pub access_inspects: u64,
    pub access_fetches: u64,
    pub access_admits: u64,
    pub access_consumption_acks: u64,
    /// 有界压缩器累计 provider 输入。优先 `ContextCompacted` 事件之和；
    /// 旧 traces 回退到 `ContextMaintained.report` 本轮花费。
    pub compaction_input_tokens: u64,
    pub compaction_output_tokens: u64,
}

/// Aggregate one run's envelopes. The caller filters to a single run.
pub fn aggregate_metrics(events: &[RuntimeEventEnvelope]) -> RunMetrics {
    let mut metrics = RunMetrics::default();
    let mut read_paths: HashSet<String> = HashSet::new();
    let mut materialize_ms_samples: Vec<u64> = Vec::new();
    let mut search_ms_samples: Vec<u64> = Vec::new();
    let mut open_calls: HashMap<String, OpenManageCall> = HashMap::new();
    let mut open_edits: HashMap<String, OpenEditCall> = HashMap::new();
    let mut edit_ms_samples: Vec<u64> = Vec::new();
    let mut successfully_edited_paths: HashSet<String> = HashSet::new();
    let mut awaiting_edit_fallback = false;
    let mut first_edit_timestamp_ms: Option<u64> = None;
    let mut forgotten: HashSet<ContextItemId> = HashSet::new();
    let mut recovered: HashSet<ContextItemId> = HashSet::new();
    let mut recovered_path: HashMap<ContextItemId, RecoverPath> = HashMap::new();
    let mut unique_reactivated: HashSet<ContextItemId> = HashSet::new();
    let mut seen_context_compacted = false;

    for envelope in events {
        match &envelope.event {
            RuntimeEvent::UserMessageAccepted { input } if input.is_applied() => metrics.turns += 1,
            RuntimeEvent::ToolStarted { call } => {
                metrics.tool_calls += 1;
                if is_edit_tool(&call.name) {
                    metrics.edit_started_calls += 1;
                    first_edit_timestamp_ms.get_or_insert(envelope.timestamp_ms);
                    open_edits.insert(
                        call.id.clone(),
                        OpenEditCall {
                            started_ms: envelope.timestamp_ms,
                            paths: argument_paths(&call.arguments),
                        },
                    );
                }
                if call.name == "fs.read"
                    && let Some(path) = call.arguments.get("path").and_then(|value| value.as_str())
                {
                    let path = normalized_path(path);
                    if !read_paths.insert(path.clone()) {
                        metrics.repeated_fs_reads += 1;
                    }
                    if successfully_edited_paths.contains(&path) {
                        metrics.post_edit_confirm_reads += 1;
                    }
                }
                if awaiting_edit_fallback {
                    match call.name.as_str() {
                        "shell.exec" => {
                            metrics.edit_failure_shell_fallback_proxy += 1;
                            awaiting_edit_fallback = false;
                        }
                        "fs.write" => {
                            metrics.edit_failure_fs_write_fallback += 1;
                            awaiting_edit_fallback = false;
                        }
                        _ => {}
                    }
                }
                if let Some(op) = manage_op(&call.name, &call.arguments) {
                    match op.as_str() {
                        "inspect" => metrics.inspect_calls += 1,
                        "fetch" => metrics.fetch_calls += 1,
                        "admit" => metrics.admit_calls += 1,
                        "search" => metrics.search_calls += 1,
                        _ => {}
                    }
                    open_calls.insert(
                        call.id.clone(),
                        OpenManageCall {
                            started_ms: envelope.timestamp_ms,
                            op,
                            item_id: call
                                .arguments
                                .get("item_id")
                                .and_then(|value| value.as_str())
                                .and_then(|raw| raw.parse().ok()),
                        },
                    );
                }
            }
            RuntimeEvent::ToolFinished { output } => {
                if !output.ok {
                    metrics.failed_tool_outputs += 1;
                }
                if output.tool_name == "fs.read"
                    && let Some(motive) = output
                        .metadata
                        .get(FS_READ_MOTIVE_KEY)
                        .and_then(|value| value.as_str())
                        .and_then(FsReadMotive::parse)
                {
                    match motive {
                        FsReadMotive::First => metrics.reread_motive_first += 1,
                        FsReadMotive::BodyVisibleCurrent => {
                            metrics.reread_motive_body_visible_current += 1
                        }
                        FsReadMotive::DescriptorOnly => metrics.reread_motive_descriptor_only += 1,
                        FsReadMotive::ProtocolCheckpointBodyMissing => {
                            metrics.reread_motive_protocol_checkpoint_body_missing += 1
                        }
                        FsReadMotive::CheckedFresh => metrics.reread_motive_checked_fresh += 1,
                        FsReadMotive::NeedsRevalidation => {
                            metrics.reread_motive_needs_revalidation += 1
                        }
                        FsReadMotive::Warm => metrics.reread_motive_warm += 1,
                        FsReadMotive::Stored => metrics.reread_motive_stored += 1,
                        FsReadMotive::Changed => metrics.reread_motive_changed += 1,
                    }
                }
                if output.tool_name == "fs.read" && output.ok {
                    metrics.fs_read_bytes_total = metrics
                        .fs_read_bytes_total
                        .saturating_add(metadata_u64(&output.metadata, "bytes"));
                }
                if is_edit_tool(&output.tool_name) {
                    metrics.edit_attempts += 1;
                    first_edit_timestamp_ms.get_or_insert(envelope.timestamp_ms);
                    if metrics.edit_first_attempts == 0 {
                        metrics.edit_first_attempts = 1;
                        if output.ok {
                            metrics.edit_first_attempt_successes = 1;
                            if output
                                .metadata
                                .get("changed")
                                .and_then(|value| value.as_bool())
                                == Some(true)
                            {
                                metrics.edit_first_attempt_committed_changes = 1;
                            }
                        }
                    }
                    let open = open_edits.remove(&output.call_id);
                    if let Some(open) = &open {
                        edit_ms_samples.push(envelope.timestamp_ms.saturating_sub(open.started_ms));
                    }
                    if output.ok {
                        metrics.edit_successes += 1;
                        if output
                            .metadata
                            .get("changed")
                            .and_then(|value| value.as_bool())
                            == Some(true)
                        {
                            metrics.edit_committed_changes += 1;
                        }
                        awaiting_edit_fallback = false;
                        let mut paths = output_paths(&output.metadata);
                        let output_named_paths = output.metadata.get("path").is_some()
                            || output.metadata.get("files").is_some();
                        if paths.is_empty()
                            && !output_named_paths
                            && let Some(open) = open
                        {
                            paths = open.paths;
                        }
                        successfully_edited_paths.extend(paths);
                        let (before, after) = successful_edit_bytes(&output.metadata);
                        metrics.edit_success_bytes_before =
                            metrics.edit_success_bytes_before.saturating_add(before);
                        metrics.edit_success_bytes_after =
                            metrics.edit_success_bytes_after.saturating_add(after);
                    } else {
                        metrics.edit_failures += 1;
                        awaiting_edit_fallback = true;
                    }
                    match output
                        .metadata
                        .get("commit_state")
                        .and_then(|value| value.as_str())
                    {
                        Some("not_applied" | "rejected") => metrics.edit_commit_not_applied += 1,
                        Some("not_applied_authority_recovery_required") => {
                            metrics.edit_commit_not_applied += 1;
                            metrics.edit_commit_recovery_required += 1;
                        }
                        Some(
                            "applied_recovery_required" | "applied_authority_recovery_required",
                        ) => metrics.edit_commit_recovery_required += 1,
                        Some(state) if state.starts_with("unknown") || state == "unsettled" => {
                            metrics.edit_commit_unknown += 1
                        }
                        _ => {}
                    }
                }
                if let Some(class) = output.failure_class() {
                    *metrics
                        .tool_failure_classes
                        .entry(class.as_str().to_string())
                        .or_default() += 1;
                }
                if output.artifact_ref.is_some() {
                    metrics.artifact_spills += 1;
                }
                metrics.output_chars_total += output.model_content.chars().count() as u64;
                let open = open_calls.remove(&output.call_id);
                let op = output
                    .metadata
                    .get("op")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
                    .or_else(|| open.as_ref().map(|call| call.op.clone()));
                if is_manage_tool(&output.tool_name) && op.as_deref() == Some("search") {
                    if open.is_none() {
                        metrics.search_calls += 1;
                    }
                    let hits = output
                        .metadata
                        .get("descriptors")
                        .and_then(|value| value.as_array())
                        .map(|rows| rows.len() as u64)
                        .unwrap_or(0);
                    metrics.search_hits += hits;
                    if hits == 0 {
                        metrics.search_empty += 1;
                    }
                    if let Some(started) = open.as_ref() {
                        search_ms_samples
                            .push(envelope.timestamp_ms.saturating_sub(started.started_ms));
                    }
                    for id in descriptor_ids(&output.metadata) {
                        mark_recovered(
                            &forgotten,
                            &mut recovered,
                            &mut recovered_path,
                            id,
                            RecoverPath::Search,
                        );
                    }
                }
                if is_manage_tool(&output.tool_name)
                    && let Some(code) = output.metadata.get("miss").and_then(|value| value.as_str())
                {
                    match code {
                        "not_found" => metrics.search_miss_not_found += 1,
                        "evidence_absent" => metrics.search_miss_evidence_absent += 1,
                        "provider_unavailable" => metrics.search_miss_provider_unavailable += 1,
                        _ => {}
                    }
                }
                if let Some(started) = open.as_ref()
                    && matches!(started.op.as_str(), "inspect" | "fetch" | "admit")
                    && let Some(id) = started.item_id
                    && forgotten.contains(&id)
                {
                    mark_recovered(
                        &forgotten,
                        &mut recovered,
                        &mut recovered_path,
                        id,
                        RecoverPath::Search,
                    );
                }
            }
            RuntimeEvent::ContextMaintained { report, .. } => {
                metrics.lifecycle_transitions += report.transitions.len() as u64;
                snapshot_access(&mut metrics, &report.diagnostics);
                // 报告字段是本轮花费；diagnostics 快照会被随后的 GC 清零。
                if !seen_context_compacted {
                    metrics.compaction_input_tokens = metrics
                        .compaction_input_tokens
                        .saturating_add(report.compaction_input_tokens);
                    metrics.compaction_output_tokens = metrics
                        .compaction_output_tokens
                        .saturating_add(report.compaction_output_tokens);
                }
            }
            RuntimeEvent::ContextCompacted {
                input_tokens,
                output_tokens,
                ..
            } => {
                if !seen_context_compacted {
                    metrics.compaction_input_tokens = 0;
                    metrics.compaction_output_tokens = 0;
                    seen_context_compacted = true;
                }
                metrics.compaction_input_tokens = metrics
                    .compaction_input_tokens
                    .saturating_add(*input_tokens);
                metrics.compaction_output_tokens = metrics
                    .compaction_output_tokens
                    .saturating_add(*output_tokens);
            }
            RuntimeEvent::ContextGc { report } => {
                metrics.gc_evictions += report.evicted as u64;
                metrics.gc_reactivations += report.reactivated as u64;
                metrics.gc_externalizations += report.externalized as u64;
                metrics.store_write_bytes_total += report.store_write_bytes;
                metrics.store_read_bytes_total += report.store_read_bytes;
                metrics.store_recalled_items_total += report.store_recalled_items;
                for eviction in &report.evictions {
                    forgotten.insert(eviction.item_id);
                }
                for id in &report.externalized_ids {
                    forgotten.insert(*id);
                }
                for reactivation in &report.reactivations {
                    mark_recovered(
                        &forgotten,
                        &mut recovered,
                        &mut recovered_path,
                        reactivation.item_id,
                        RecoverPath::Reactivate,
                    );
                    unique_reactivated.insert(reactivation.item_id);
                    metrics.reactivation_events = metrics.reactivation_events.saturating_add(1);
                }
                snapshot_access(&mut metrics, &report.diagnostics);
            }
            RuntimeEvent::ToolSurfacePlanned { report } => {
                metrics.rounds += 1;
                metrics.schema_tokens_total += report.selected_schema_tokens as u64;
            }
            RuntimeEvent::ModelStarted { prompt_layers, .. } => {
                metrics.prompt_system_tokens = metrics
                    .prompt_system_tokens
                    .saturating_add(prompt_layers.system_tokens);
                metrics.prompt_runtime_facts_tokens = metrics
                    .prompt_runtime_facts_tokens
                    .saturating_add(prompt_layers.runtime_facts_tokens);
                metrics.prompt_task_anchor_tokens = metrics
                    .prompt_task_anchor_tokens
                    .saturating_add(prompt_layers.task_anchor_tokens);
                metrics.prompt_task_progress_tokens = metrics
                    .prompt_task_progress_tokens
                    .saturating_add(prompt_layers.task_progress_tokens);
                metrics.prompt_current_focus_tokens = metrics
                    .prompt_current_focus_tokens
                    .saturating_add(prompt_layers.current_focus_tokens);
                metrics.prompt_historical_context_tokens = metrics
                    .prompt_historical_context_tokens
                    .saturating_add(prompt_layers.historical_context_tokens);
                metrics.prompt_turn_frame_tokens = metrics
                    .prompt_turn_frame_tokens
                    .saturating_add(prompt_layers.turn_frame_tokens);
                metrics.prompt_tool_schema_tokens = metrics
                    .prompt_tool_schema_tokens
                    .saturating_add(prompt_layers.tool_schema_tokens);
                metrics.prompt_tool_catalog_index_tokens = metrics
                    .prompt_tool_catalog_index_tokens
                    .saturating_add(prompt_layers.tool_catalog_index_tokens);
            }
            RuntimeEvent::ModelUsed {
                input_tokens,
                output_tokens,
                attempts,
                retries,
            } => {
                metrics.model_input_tokens += input_tokens;
                metrics.model_output_tokens += output_tokens;
                let attempts = (*attempts).max(1) as u64;
                metrics.model_attempts = metrics.model_attempts.saturating_add(attempts);
                metrics.model_retries = metrics.model_retries.saturating_add(*retries as u64);
                if *retries > 0 {
                    metrics.provider_tokens_lower_bound = true;
                }
            }
            RuntimeEvent::ContextPrepared {
                diagnostics,
                selected,
                materialize_ms,
            } => {
                metrics.materialize_rounds += 1;
                metrics.selected_items_total += selected.len() as u64;
                for item in selected {
                    let tokens = item.approx_tokens as u64;
                    metrics.selected_tokens_total += tokens;
                    let kind = item
                        .kind
                        .map(|kind| kind.as_str())
                        .unwrap_or("unknown")
                        .to_string();
                    *metrics.selected_tokens_by_kind.entry(kind).or_default() += tokens;
                    let reason = selection_reason_class(&item.reason).to_string();
                    *metrics.selected_tokens_by_reason.entry(reason).or_default() += tokens;
                    let source = item
                        .source
                        .as_deref()
                        .filter(|source| !source.is_empty())
                        .unwrap_or("unknown")
                        .to_string();
                    *metrics.selected_tokens_by_source.entry(source).or_default() += tokens;
                    if item.reactivated {
                        metrics.selected_tokens_reactivated += tokens;
                    } else {
                        metrics.selected_tokens_resident += tokens;
                    }
                }
                metrics.active_tokens_total += diagnostics.approx_active_tokens as u64;
                if *materialize_ms > 0 {
                    materialize_ms_samples.push(*materialize_ms);
                }
                metrics.final_total_items = diagnostics.total_items as u64;
                metrics.final_resident_items = diagnostics.resident_items as u64;
                metrics.final_warm_items = diagnostics.warm_items as u64;
                metrics.final_cold_items = diagnostics.cold_items as u64;
                metrics.final_external_items = diagnostics.external_items as u64;
                metrics.final_resident_bytes = diagnostics.resident_bytes as u64;
                metrics.peak_resident_bytes = metrics
                    .peak_resident_bytes
                    .max(diagnostics.resident_bytes as u64);
                snapshot_access(&mut metrics, diagnostics);
            }
            _ => {}
        }
    }
    if !materialize_ms_samples.is_empty() {
        materialize_ms_samples.sort_unstable();
        metrics.materialize_ms_p50 = percentile(&materialize_ms_samples, 50);
        metrics.materialize_ms_p95 = percentile(&materialize_ms_samples, 95);
    }
    if !search_ms_samples.is_empty() {
        search_ms_samples.sort_unstable();
        metrics.search_ms_p50 = percentile(&search_ms_samples, 50);
        metrics.search_ms_p95 = percentile(&search_ms_samples, 95);
    }
    if !edit_ms_samples.is_empty() {
        edit_ms_samples.sort_unstable();
        metrics.edit_ms_p50 = percentile(&edit_ms_samples, 50);
        metrics.edit_ms_p95 = percentile(&edit_ms_samples, 95);
    }
    metrics.edit_unfinished_calls = open_edits.len() as u64;
    if let (Some(first), Some(last)) = (first_edit_timestamp_ms, events.last()) {
        metrics.edit_to_trace_end_ms = last.timestamp_ms.saturating_sub(first);
    }
    metrics.forgotten_items = forgotten.len() as u64;
    metrics.recovered_items = recovered.len() as u64;
    metrics.recovery_explicit_search = recovered_path
        .values()
        .filter(|path| **path == RecoverPath::Search)
        .count() as u64;
    metrics.recovery_auto_reactivation = recovered_path
        .values()
        .filter(|path| **path == RecoverPath::Reactivate)
        .count() as u64;
    metrics.recovery_workspace_reread = metrics.repeated_fs_reads;
    metrics.recovery_failed = forgotten.difference(&recovered).count() as u64;
    metrics.unique_reactivated = unique_reactivated.len() as u64;
    metrics
}

fn mark_recovered(
    forgotten: &HashSet<ContextItemId>,
    recovered: &mut HashSet<ContextItemId>,
    recovered_path: &mut HashMap<ContextItemId, RecoverPath>,
    id: ContextItemId,
    path: RecoverPath,
) {
    if forgotten.contains(&id) {
        recovered.insert(id);
        recovered_path.entry(id).or_insert(path);
    }
}

struct OpenManageCall {
    started_ms: u64,
    op: String,
    item_id: Option<ContextItemId>,
}

struct OpenEditCall {
    started_ms: u64,
    paths: Vec<String>,
}

fn is_edit_tool(name: &str) -> bool {
    matches!(name, "edit.replace" | "edit.patch")
}

fn normalized_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn argument_paths(arguments: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = arguments.get("path").and_then(|value| value.as_str()) {
        paths.push(normalized_path(path));
    }
    if let Some(files) = arguments.get("files").and_then(|value| value.as_array()) {
        for file in files {
            if let Some(path) = file.get("path").and_then(|value| value.as_str()) {
                let path = normalized_path(path);
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

fn output_paths(metadata: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    if metadata
        .get("changed")
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
        && let Some(path) = metadata.get("path").and_then(|value| value.as_str())
    {
        paths.push(normalized_path(path));
    }
    if let Some(files) = metadata.get("files").and_then(|value| value.as_array()) {
        for file in files {
            if !file
                .get("changed")
                .and_then(|value| value.as_bool())
                .unwrap_or(true)
            {
                continue;
            }
            if let Some(path) = file.get("path").and_then(|value| value.as_str()) {
                let path = normalized_path(path);
                if !paths.contains(&path) {
                    paths.push(path);
                }
            }
        }
    }
    paths
}

fn metadata_u64(metadata: &serde_json::Value, key: &str) -> u64 {
    metadata
        .get(key)
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
}

fn successful_edit_bytes(metadata: &serde_json::Value) -> (u64, u64) {
    if let Some(files) = metadata.get("files").and_then(|value| value.as_array()) {
        return files
            .iter()
            .filter(|file| {
                file.get("changed")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(true)
            })
            .fold((0_u64, 0_u64), |(before, after), file| {
                (
                    before.saturating_add(metadata_u64(file, "bytes_before")),
                    after.saturating_add(metadata_u64(file, "bytes_after")),
                )
            });
    }
    (
        metadata_u64(metadata, "bytes_before"),
        metadata_u64(metadata, "bytes_after"),
    )
}

fn is_manage_tool(name: &str) -> bool {
    name == CONTEXT_MANAGE || name == CAPABILITY_MANAGE
}

fn manage_op(tool_name: &str, arguments: &serde_json::Value) -> Option<String> {
    if !is_manage_tool(tool_name) {
        return None;
    }
    arguments
        .get("op")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// 从 discovery 描述符卡抽出可解析的 context item id。
/// tool 名不会通过 `ContextItemId` 解析，因此不会误计为 recovered。
fn descriptor_ids(metadata: &serde_json::Value) -> Vec<ContextItemId> {
    let Some(rows) = metadata
        .get("descriptors")
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            row.get("ref")
                .and_then(|value| value.get("id"))
                .and_then(|value| value.as_str())
                .and_then(|raw| raw.parse().ok())
        })
        .collect()
}

/// 以最后一次 diagnostics 快照覆盖分级戳计数（累计值，不是增量）。
fn snapshot_access(metrics: &mut RunMetrics, diagnostics: &agent_contracts::ContextDiagnostics) {
    metrics.access_search_hits = diagnostics.access_search_hits;
    metrics.access_inspects = diagnostics.access_inspects;
    metrics.access_fetches = diagnostics.access_fetches;
    metrics.access_admits = diagnostics.access_admits;
    metrics.access_consumption_acks = diagnostics.access_consumption_acks;
    metrics.reactivation_selected = diagnostics.reactivation_selected;
    metrics.reactivation_consumed = diagnostics.reactivation_consumed;
    metrics.reactivation_selected_tokens = diagnostics.reactivation_selected_tokens;
    metrics.reactivation_consumed_tokens = diagnostics.reactivation_consumed_tokens;
    // reactivation_events / unique_reactivated are summed from ContextGc
    // events (run-global). Engine diagnostics are segment-local and reset
    // on restore, so they must not overwrite the event aggregate.
    metrics.reactivated_tokens = diagnostics.reactivated_tokens;
    metrics.reactivation_tool_observation_selected =
        diagnostics.reactivation_tool_observation_selected;
    metrics.reactivation_tool_observation_consumed =
        diagnostics.reactivation_tool_observation_consumed;
    metrics.reactivation_file_observation_selected =
        diagnostics.reactivation_file_observation_selected;
    metrics.reactivation_file_observation_consumed =
        diagnostics.reactivation_file_observation_consumed;
    metrics.reread_previously_selected = diagnostics.reread_previously_selected;
    metrics.reread_selected_descriptor = diagnostics.reread_selected_descriptor;
    metrics.reread_external_descriptor = diagnostics.reread_external_descriptor;
    metrics.reread_resident_unselected = diagnostics.reread_resident_unselected;
    metrics.reread_warm = diagnostics.reread_warm;
    metrics.reread_stored = diagnostics.reread_stored;
    metrics.reread_first_read = diagnostics.reread_first_read;
}

/// The nearest-index percentile of a sorted sample: index
/// `round((len - 1) * pct / 100)`.
pub(crate) fn percentile(sorted: &[u64], pct: u64) -> u64 {
    debug_assert!(!sorted.is_empty());
    let index = ((sorted.len() - 1) as f64 * pct as f64 / 100.0).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

fn selection_reason_class(reason: &str) -> &'static str {
    if reason.starts_with("explicitly pinned") {
        "pinned"
    } else if reason.starts_with("anchor root") {
        "anchor_prompt"
    } else if reason.starts_with("included as dependency") {
        "dependency"
    } else if reason.starts_with("latest body") {
        "latest_file_body"
    } else {
        "scored"
    }
}

/// Human-readable metric block for one run.
pub fn render_metrics(metrics: &RunMetrics) -> String {
    format!(
        "cost: model_in={} model_out={} attempts={} retries={} tokens_lower_bound={} schema_tokens={} rounds={} turns={} lifecycle_transitions={}\n\
         gc: evictions={} reactivations={} externalizations={}\n\
         materialize: rounds={} selected_items={} selected_tokens={} active_tokens={}\n\
         materialize_latency: p50={}ms p95={}ms\n\
         residency(final): total={} resident={} warm={} cold={} external={}\n\
         resident_bytes: final={} peak={}\n\
         store: write_bytes={} read_bytes={} recalled_items={}\n\
         retrieval: search_calls={} hits={} empty={} miss(not_found/absent/unavailable)={}/{}/{}\n\
         retrieval_latency: p50={}ms p95={}ms inspect={} fetch={} admit={}\n\
         recovery: forgotten={} recovered={} search={} reactivate={} reread={} failed={}\n\
         access: search_hits={} inspects={} fetches={} admits={} acks={}\n\
         reactivation_utility: events={} unique={} selected={} consumed={} tokens(reactivated/selected/consumed)={}/{}/{}\n\
         reactivation_kind: tool_obs selected/consumed={}/{} file_obs selected/consumed={}/{}\n\
         prompt_layers: system={} facts={} anchor={} progress={} focus={} history={} turn={} tools={} catalog={}\n\
         compaction: in={} out={}\n\
         behavior: tool_calls={} failed_outputs={} spills={} output_chars={} repeated_fs_reads={}\n\
         edits: attempts={} started={} raw_ok={} committed_change={} failed={} first_raw_ok={}/{} first_committed_change={}/{} unfinished={} latency_p50={}ms latency_p95={}ms to_trace_end={}ms\n\
         edit_io: fs_read_bytes={} success_bytes_before={} success_bytes_after={} confirm_reads={} fallback(shell_proxy/fs_write)={}/{} settlement(not_applied/recovery/unknown)={}/{}/{}\n\
         reread: previously_selected={} selected_descriptor={} external_descriptor={} resident_unselected={} warm={} stored={} first_read={}\n\
         reread_motive: first={} body_visible_current={} descriptor_only={} protocol_checkpoint_body_missing={} checked_fresh={} needs_revalidation={} warm={} stored={} changed={}\n\
         selected_attr: kind={:?} reason={:?} source={:?} reactivated={} resident={}\n\
         tool_failures: {:?}\n",
        metrics.model_input_tokens,
        metrics.model_output_tokens,
        metrics.model_attempts,
        metrics.model_retries,
        metrics.provider_tokens_lower_bound,
        metrics.schema_tokens_total,
        metrics.rounds,
        metrics.turns,
        metrics.lifecycle_transitions,
        metrics.gc_evictions,
        metrics.gc_reactivations,
        metrics.gc_externalizations,
        metrics.materialize_rounds,
        metrics.selected_items_total,
        metrics.selected_tokens_total,
        metrics.active_tokens_total,
        metrics.materialize_ms_p50,
        metrics.materialize_ms_p95,
        metrics.final_total_items,
        metrics.final_resident_items,
        metrics.final_warm_items,
        metrics.final_cold_items,
        metrics.final_external_items,
        metrics.final_resident_bytes,
        metrics.peak_resident_bytes,
        metrics.store_write_bytes_total,
        metrics.store_read_bytes_total,
        metrics.store_recalled_items_total,
        metrics.search_calls,
        metrics.search_hits,
        metrics.search_empty,
        metrics.search_miss_not_found,
        metrics.search_miss_evidence_absent,
        metrics.search_miss_provider_unavailable,
        metrics.search_ms_p50,
        metrics.search_ms_p95,
        metrics.inspect_calls,
        metrics.fetch_calls,
        metrics.admit_calls,
        metrics.forgotten_items,
        metrics.recovered_items,
        metrics.recovery_explicit_search,
        metrics.recovery_auto_reactivation,
        metrics.recovery_workspace_reread,
        metrics.recovery_failed,
        metrics.access_search_hits,
        metrics.access_inspects,
        metrics.access_fetches,
        metrics.access_admits,
        metrics.access_consumption_acks,
        metrics.reactivation_events,
        metrics.unique_reactivated,
        metrics.reactivation_selected,
        metrics.reactivation_consumed,
        metrics.reactivated_tokens,
        metrics.reactivation_selected_tokens,
        metrics.reactivation_consumed_tokens,
        metrics.reactivation_tool_observation_selected,
        metrics.reactivation_tool_observation_consumed,
        metrics.reactivation_file_observation_selected,
        metrics.reactivation_file_observation_consumed,
        metrics.prompt_system_tokens,
        metrics.prompt_runtime_facts_tokens,
        metrics.prompt_task_anchor_tokens,
        metrics.prompt_task_progress_tokens,
        metrics.prompt_current_focus_tokens,
        metrics.prompt_historical_context_tokens,
        metrics.prompt_turn_frame_tokens,
        metrics.prompt_tool_schema_tokens,
        metrics.prompt_tool_catalog_index_tokens,
        metrics.compaction_input_tokens,
        metrics.compaction_output_tokens,
        metrics.tool_calls,
        metrics.failed_tool_outputs,
        metrics.artifact_spills,
        metrics.output_chars_total,
        metrics.repeated_fs_reads,
        metrics.edit_attempts,
        metrics.edit_started_calls,
        metrics.edit_successes,
        metrics.edit_committed_changes,
        metrics.edit_failures,
        metrics.edit_first_attempt_successes,
        metrics.edit_first_attempts,
        metrics.edit_first_attempt_committed_changes,
        metrics.edit_first_attempts,
        metrics.edit_unfinished_calls,
        metrics.edit_ms_p50,
        metrics.edit_ms_p95,
        metrics.edit_to_trace_end_ms,
        metrics.fs_read_bytes_total,
        metrics.edit_success_bytes_before,
        metrics.edit_success_bytes_after,
        metrics.post_edit_confirm_reads,
        metrics.edit_failure_shell_fallback_proxy,
        metrics.edit_failure_fs_write_fallback,
        metrics.edit_commit_not_applied,
        metrics.edit_commit_recovery_required,
        metrics.edit_commit_unknown,
        metrics.reread_previously_selected,
        metrics.reread_selected_descriptor,
        metrics.reread_external_descriptor,
        metrics.reread_resident_unselected,
        metrics.reread_warm,
        metrics.reread_stored,
        metrics.reread_first_read,
        metrics.reread_motive_first,
        metrics.reread_motive_body_visible_current,
        metrics.reread_motive_descriptor_only,
        metrics.reread_motive_protocol_checkpoint_body_missing,
        metrics.reread_motive_checked_fresh,
        metrics.reread_motive_needs_revalidation,
        metrics.reread_motive_warm,
        metrics.reread_motive_stored,
        metrics.reread_motive_changed,
        metrics.selected_tokens_by_kind,
        metrics.selected_tokens_by_reason,
        metrics.selected_tokens_by_source,
        metrics.selected_tokens_reactivated,
        metrics.selected_tokens_resident,
        metrics.tool_failure_classes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        AttentionState, CompactionReason, ContextDiagnostics, ContextGcReport, ContextItemId,
        ContextKind, ContextMaintenanceReport, ContextMaintenanceTrigger, ContextScope,
        ContextSelection, ContextStateTransition, InputLifecycle, OperationId, PromptLayerCosts,
        RunId, RuntimeInputEnvelope, ScoreBreakdown, TaskId, ToolOutput, ToolSurfaceDemand,
        ToolSurfacePlanReport, ToolSurfacePlanStatus, ToolSurfaceSelection,
        ToolSurfaceSourceRevisions, TurnId,
    };
    use serde_json::json;

    fn envelope(run: RunId, seq: u64, event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id: run,
            seq,
            timestamp_ms: seq,
            event,
        }
    }

    fn transition() -> ContextStateTransition {
        ContextStateTransition {
            item_id: ContextItemId::new(),
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            from: AttentionState::Active,
            to: AttentionState::Archived,
            turn: 1,
            reason: "test".into(),
        }
    }

    #[test]
    fn aggregates_every_measurable_dimension() {
        let run = RunId::new();
        let mut seq = 1;
        let mut events = Vec::new();

        events.push(envelope(run, seq, RuntimeEvent::RunStarted));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::FocusChanged {
                task_id: TaskId::new(),
                goal: "task".into(),
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::user_message_accepted("fix it"),
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolStarted {
                call: agent_contracts::ToolCall {
                    id: "1".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "src/main.rs"}),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: "1".into(),
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "read".into(),
                    model_content: "content".into(),
                    artifact_ref: Some("artifact://run/full.txt".into()),
                    metadata: json!({}),
                },
            },
        ));
        seq += 1;
        // A repeated read of the same path counts.
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolStarted {
                call: agent_contracts::ToolCall {
                    id: "2".into(),
                    name: "fs.read".into(),
                    arguments: json!({"path": "src/main.rs"}),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: "2".into(),
                    tool_name: "fs.read".into(),
                    ok: false,
                    summary: "failed".into(),
                    model_content: String::new(),
                    artifact_ref: None,
                    metadata: json!({"failure_class": "no_exact_match"}),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextMaintained {
                trigger: ContextMaintenanceTrigger::UserInput,
                report: ContextMaintenanceReport {
                    promoted: 1,
                    cooled: 2,
                    archived: 1,
                    tombstoned: 0,
                    turn: 1,
                    transitions: vec![transition(), transition()],
                    diagnostics: Default::default(),
                    ..ContextMaintenanceReport::default()
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextGc {
                report: ContextGcReport {
                    marked_roots: 3,
                    resident: 5,
                    evicted: 2,
                    reactivated: 1,
                    externalized: 1,
                    aged_external: 0,
                    anchor_roots_protected: 1,
                    evictions: Vec::new(),
                    reactivations: Vec::new(),
                    store_blob_delete_errors: 0,
                    store_write_bytes: 512,
                    store_read_bytes: 128,
                    store_recalled_items: 2,
                    diagnostics: Default::default(),
                    externalized_ids: Vec::new(),
                    anchor_root_protections: Vec::new(),
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ToolSurfacePlanned {
                report: ToolSurfacePlanReport {
                    turn_id: TurnId::new(),
                    model_round: 1,
                    surface_revision: 1,
                    source_revisions: ToolSurfaceSourceRevisions::default(),
                    status: ToolSurfacePlanStatus::Ready,
                    selected: vec![ToolSurfaceSelection {
                        tool_name: "fs.read".into(),
                        demand: ToolSurfaceDemand::MustSurface,
                        origin: agent_contracts::ToolSurfaceOrigin::DispatcherRequired,
                        approx_tokens: 100,
                    }],
                    selected_total: 1,
                    omitted: Vec::new(),
                    omitted_total: 0,
                    blocked: Vec::new(),
                    blocked_total: 0,
                    selected_schema_tokens: 512,
                    mandatory_schema_tokens: 512,
                    estimated_input_tokens: 10_000,
                    input_budget_tokens: 24_000,
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ModelStarted {
                turn_id: TurnId::new(),
                operation_id: OperationId::new(),
                generation: 1,
                surface_revision: 1,
                model_round: 1,
                prompt_layers: PromptLayerCosts {
                    system_tokens: 80,
                    runtime_facts_tokens: 20,
                    task_anchor_tokens: 30,
                    task_progress_tokens: 40,
                    current_focus_tokens: 15,
                    historical_context_tokens: 800,
                    turn_frame_tokens: 50,
                    tool_schema_tokens: 512,
                    tool_catalog_index_tokens: 40,
                },
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ModelUsed {
                input_tokens: 9_000,
                output_tokens: 120,
                attempts: 1,
                retries: 0,
            },
        ));
        seq += 1;
        // Two materialization previews: cumulative sums add up, the final
        // residency snapshot is the last preview's, and the latency
        // percentiles come from the timestamped samples.
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextPrepared {
                diagnostics: ContextDiagnostics {
                    total_items: 10,
                    resident_items: 6,
                    resident_bytes: 1_200,
                    warm_items: 2,
                    cold_items: 1,
                    external_items: 1,
                    approx_active_tokens: 5_000,
                    ..ContextDiagnostics::default()
                },
                selected: vec![
                    ContextSelection {
                        item_id: ContextItemId::new(),
                        score: 1.0,
                        approx_tokens: 300,
                        reason: "focus".into(),
                        breakdown: ScoreBreakdown::default(),
                        ..Default::default()
                    },
                    ContextSelection {
                        item_id: ContextItemId::new(),
                        score: 0.5,
                        approx_tokens: 200,
                        reason: "recall".into(),
                        breakdown: ScoreBreakdown::default(),
                        ..Default::default()
                    },
                ],
                materialize_ms: 10,
            },
        ));
        seq += 1;
        events.push(envelope(
            run,
            seq,
            RuntimeEvent::ContextPrepared {
                diagnostics: ContextDiagnostics {
                    total_items: 11,
                    resident_items: 7,
                    resident_bytes: 1_800,
                    warm_items: 2,
                    cold_items: 1,
                    external_items: 1,
                    approx_active_tokens: 4_000,
                    reactivation_events: 42,
                    unique_reactivated: 32,
                    reactivation_selected: 10,
                    reactivation_consumed: 8,
                    reactivated_tokens: 400,
                    reactivation_selected_tokens: 120,
                    reactivation_consumed_tokens: 90,
                    reactivation_tool_observation_selected: 6,
                    reactivation_tool_observation_consumed: 4,
                    reactivation_file_observation_selected: 3,
                    reactivation_file_observation_consumed: 2,
                    ..ContextDiagnostics::default()
                },
                selected: vec![ContextSelection {
                    item_id: ContextItemId::new(),
                    score: 0.9,
                    approx_tokens: 250,
                    reason: "focus".into(),
                    breakdown: ScoreBreakdown::default(),
                    ..Default::default()
                }],
                materialize_ms: 20,
            },
        ));
        seq += 1;
        events.push(envelope(run, seq, RuntimeEvent::TurnCompleted));

        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.turns, 1);
        assert_eq!(metrics.tool_calls, 2);
        assert_eq!(metrics.repeated_fs_reads, 1);
        assert_eq!(metrics.failed_tool_outputs, 1);
        assert_eq!(
            metrics.tool_failure_classes.get("no_exact_match").copied(),
            Some(1)
        );
        assert_eq!(metrics.artifact_spills, 1);
        assert_eq!(metrics.output_chars_total, 7, "content + failed(0)");
        assert_eq!(metrics.lifecycle_transitions, 2);
        assert_eq!(metrics.gc_evictions, 2);
        assert_eq!(metrics.gc_reactivations, 1);
        assert_eq!(metrics.gc_externalizations, 1);
        assert_eq!(metrics.rounds, 1);
        assert_eq!(metrics.schema_tokens_total, 512);
        assert_eq!(metrics.model_input_tokens, 9_000);
        assert_eq!(metrics.model_output_tokens, 120);
        assert_eq!(metrics.model_attempts, 1);
        assert!(!metrics.provider_tokens_lower_bound);
        assert_eq!(metrics.prompt_task_progress_tokens, 40);
        assert_eq!(metrics.prompt_historical_context_tokens, 800);
        assert_eq!(metrics.prompt_tool_catalog_index_tokens, 40);
        assert_eq!(metrics.reactivation_events, 0);
        assert_eq!(metrics.unique_reactivated, 0);
        assert_eq!(metrics.reactivation_selected, 10);
        assert_eq!(metrics.reactivation_tool_observation_selected, 6);
        assert_eq!(metrics.materialize_rounds, 2);
        assert_eq!(metrics.selected_items_total, 3);
        assert_eq!(metrics.selected_tokens_total, 750);
        assert_eq!(metrics.selected_tokens_resident, 750);
        assert_eq!(
            metrics.selected_tokens_by_reason.get("scored").copied(),
            Some(750)
        );
        assert_eq!(metrics.active_tokens_total, 9_000);
        assert_eq!(metrics.final_total_items, 11);
        assert_eq!(metrics.final_resident_items, 7);
        assert_eq!(metrics.final_warm_items, 2);
        assert_eq!(metrics.final_cold_items, 1);
        assert_eq!(metrics.final_external_items, 1);
        assert_eq!(metrics.final_resident_bytes, 1_800);
        assert_eq!(metrics.peak_resident_bytes, 1_800);
        assert_eq!(metrics.store_write_bytes_total, 512);
        assert_eq!(metrics.store_read_bytes_total, 128);
        assert_eq!(metrics.store_recalled_items_total, 2);
        // Two samples [10, 20]: the nearest-rank p50/p95 both land on the
        // second sample.
        assert_eq!(metrics.materialize_ms_p50, 20);
        assert_eq!(metrics.materialize_ms_p95, 20);

        let rendered = render_metrics(&metrics);
        assert!(rendered.contains("model_in=9000"));
        assert!(rendered.contains("repeated_fs_reads=1"));
        assert!(rendered.contains("materialize: rounds=2 selected_items=3 selected_tokens=750"));
        assert!(
            rendered.contains("residency(final): total=11 resident=7 warm=2 cold=1 external=1")
        );
        assert!(rendered.contains("resident_bytes: final=1800 peak=1800"));
        assert!(rendered.contains("materialize_latency: p50=20ms p95=20ms"));
        assert!(rendered.contains("store: write_bytes=512 read_bytes=128 recalled_items=2"));
        assert!(rendered.contains("retrieval: search_calls=0"));
        assert!(rendered.contains("recovery: forgotten=0 recovered=0"));
        assert!(rendered.contains("events=0 unique=0"));
        assert!(rendered.contains("progress=40"));
        assert!(rendered.contains("history=800"));
        assert!(rendered.contains("tool_obs selected/consumed=6/4"));
    }

    #[test]
    fn retrieval_metrics_join_forgotten_ids_and_search_hits() {
        let run = RunId::new();
        let forgotten = ContextItemId::new();
        let call_id = "search-1";
        let events = vec![
            RuntimeEventEnvelope {
                run_id: run,
                seq: 1,
                timestamp_ms: 10,
                event: RuntimeEvent::ToolStarted {
                    call: agent_contracts::ToolCall {
                        id: call_id.into(),
                        name: agent_contracts::CONTEXT_MANAGE.into(),
                        arguments: json!({"op": "search", "query": "AuthService"}),
                    },
                },
            },
            RuntimeEventEnvelope {
                run_id: run,
                seq: 2,
                timestamp_ms: 12,
                event: RuntimeEvent::ContextGc {
                    report: ContextGcReport {
                        externalized: 1,
                        externalized_ids: vec![forgotten],
                        diagnostics: agent_contracts::ContextDiagnostics {
                            access_search_hits: 0,
                            ..Default::default()
                        },
                        ..ContextGcReport::default()
                    },
                },
            },
            RuntimeEventEnvelope {
                run_id: run,
                seq: 3,
                timestamp_ms: 50,
                event: RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: call_id.into(),
                        tool_name: agent_contracts::CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "1 external ref(s) match".into(),
                        model_content: "hit".into(),
                        artifact_ref: None,
                        metadata: json!({
                            "op": "search",
                            "kind": "context",
                            "descriptors": [{
                                "ref": {
                                    "version": 1,
                                    "kind": "context",
                                    "id": forgotten.to_string()
                                },
                                "title": "AuthService",
                                "summary": "AuthService",
                                "owner": "context",
                                "lifecycle": "Cold"
                            }]
                        }),
                    },
                },
            },
        ];
        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.search_calls, 1);
        assert_eq!(metrics.search_hits, 1);
        assert_eq!(metrics.search_empty, 0);
        assert_eq!(metrics.search_ms_p50, 40);
        assert_eq!(metrics.forgotten_items, 1);
        assert_eq!(metrics.recovered_items, 1);
        assert_eq!(metrics.recovery_explicit_search, 1);
        assert_eq!(metrics.recovery_auto_reactivation, 0);
        assert_eq!(metrics.recovery_failed, 0);
        let rendered = render_metrics(&metrics);
        assert!(rendered.contains("retrieval: search_calls=1 hits=1"));
        assert!(rendered.contains("recovery: forgotten=1 recovered=1"));
    }

    #[test]
    fn edit_metrics_pair_calls_and_expose_recovery_overhead() {
        let run = RunId::new();
        let event = |seq, timestamp_ms, event| RuntimeEventEnvelope {
            run_id: run,
            seq,
            timestamp_ms,
            event,
        };
        let events = vec![
            event(
                1,
                10,
                RuntimeEvent::ToolStarted {
                    call: agent_contracts::ToolCall {
                        id: "edit-1".into(),
                        name: "edit.replace".into(),
                        arguments: json!({"path": "src\\lib.rs", "old": "a", "new": "b"}),
                    },
                },
            ),
            event(
                2,
                20,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "edit-1".into(),
                        tool_name: "edit.replace".into(),
                        ok: false,
                        summary: "commit failed".into(),
                        model_content: String::new(),
                        artifact_ref: None,
                        metadata: json!({
                            "commit_state": "not_applied",
                            "attempted_paths": ["src/lib.rs"]
                        }),
                    },
                },
            ),
            event(
                3,
                21,
                RuntimeEvent::ToolStarted {
                    call: agent_contracts::ToolCall {
                        id: "fallback".into(),
                        name: "shell.exec".into(),
                        arguments: json!({"command": "git diff"}),
                    },
                },
            ),
            event(
                4,
                30,
                RuntimeEvent::ToolStarted {
                    call: agent_contracts::ToolCall {
                        id: "edit-2".into(),
                        name: "edit.patch".into(),
                        arguments: json!({
                            "files": [{"path": "src/lib.rs", "hunks": []}]
                        }),
                    },
                },
            ),
            event(
                5,
                50,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "edit-2".into(),
                        tool_name: "edit.patch".into(),
                        ok: true,
                        summary: "patched".into(),
                        model_content: "done".into(),
                        artifact_ref: None,
                        metadata: json!({
                            "changed": true,
                            "files": [{
                                "path": "src/lib.rs",
                                "changed": true,
                                "bytes_before": 100,
                                "bytes_after": 120
                            }]
                        }),
                    },
                },
            ),
            event(
                6,
                60,
                RuntimeEvent::ToolStarted {
                    call: agent_contracts::ToolCall {
                        id: "read-1".into(),
                        name: "fs.read".into(),
                        arguments: json!({"path": "src/lib.rs"}),
                    },
                },
            ),
            event(
                7,
                70,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "read-1".into(),
                        tool_name: "fs.read".into(),
                        ok: true,
                        summary: "read".into(),
                        model_content: "body".into(),
                        artifact_ref: None,
                        metadata: json!({"path": "src/lib.rs", "bytes": 120}),
                    },
                },
            ),
            event(
                8,
                80,
                RuntimeEvent::ToolStarted {
                    call: agent_contracts::ToolCall {
                        id: "edit-cancelled".into(),
                        name: "edit.replace".into(),
                        arguments: json!({"path": "src/lib.rs", "old": "b", "new": "c"}),
                    },
                },
            ),
            event(9, 90, RuntimeEvent::TurnCompleted),
        ];

        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.edit_attempts, 2);
        assert_eq!(metrics.edit_started_calls, 3);
        assert_eq!(metrics.edit_successes, 1);
        assert_eq!(metrics.edit_committed_changes, 1);
        assert_eq!(metrics.edit_failures, 1);
        assert_eq!(metrics.edit_first_attempts, 1);
        assert_eq!(metrics.edit_first_attempt_successes, 0);
        assert_eq!(metrics.edit_first_attempt_committed_changes, 0);
        assert_eq!(metrics.edit_unfinished_calls, 1);
        assert_eq!(metrics.edit_ms_p50, 20);
        assert_eq!(metrics.edit_ms_p95, 20);
        assert_eq!(metrics.edit_to_trace_end_ms, 80);
        assert_eq!(metrics.fs_read_bytes_total, 120);
        assert_eq!(metrics.edit_success_bytes_before, 100);
        assert_eq!(metrics.edit_success_bytes_after, 120);
        assert_eq!(metrics.post_edit_confirm_reads, 1);
        assert_eq!(metrics.edit_failure_shell_fallback_proxy, 1);
        assert_eq!(metrics.edit_failure_fs_write_fallback, 0);
        assert_eq!(metrics.edit_commit_not_applied, 1);
        assert_eq!(metrics.edit_commit_recovery_required, 0);
        assert_eq!(metrics.edit_commit_unknown, 0);
        assert!(render_metrics(&metrics).contains("first_raw_ok=0/1"));
    }

    #[test]
    fn edit_commit_metrics_cover_every_runtime_settlement_label() {
        let run = RunId::new();
        let states = [
            "not_applied",
            "applied_recovery_required",
            "unknown_recovery_required",
            "not_applied_authority_recovery_required",
            "applied_authority_recovery_required",
            "unknown_authority_recovery_required",
            "rejected",
            "unsettled",
        ];
        let events: Vec<_> = states
            .iter()
            .enumerate()
            .map(|(index, state)| RuntimeEventEnvelope {
                run_id: run,
                seq: index as u64 + 1,
                timestamp_ms: index as u64,
                event: RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: format!("edit-{index}"),
                        tool_name: "edit.patch".into(),
                        ok: false,
                        summary: "settlement".into(),
                        model_content: String::new(),
                        artifact_ref: None,
                        metadata: json!({"commit_state": state}),
                    },
                },
            })
            .collect();

        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.edit_commit_not_applied, 3);
        assert_eq!(metrics.edit_commit_recovery_required, 3);
        assert_eq!(metrics.edit_commit_unknown, 3);
    }

    #[test]
    fn first_recovery_path_is_reactivation_not_a_later_search() {
        let run = RunId::new();
        let forgotten = ContextItemId::new();
        let events = vec![
            envelope(
                run,
                1,
                RuntimeEvent::ContextGc {
                    report: ContextGcReport {
                        evicted: 1,
                        evictions: vec![agent_contracts::ContextEviction {
                            item_id: forgotten,
                            kind: ContextKind::Note,
                            scope: ContextScope::Task,
                            generation: 1,
                            evicted_at_tick: 1,
                            reason: "stale".into(),
                        }],
                        reactivations: vec![agent_contracts::ContextReactivation {
                            item_id: forgotten,
                            kind: ContextKind::Note,
                            scope: ContextScope::Task,
                            reactivated_at_tick: 2,
                            reason: "hot entity".into(),
                        }],
                        reactivated: 1,
                        diagnostics: ContextDiagnostics::default(),
                        ..ContextGcReport::default()
                    },
                },
            ),
            envelope(
                run,
                2,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "s".into(),
                        tool_name: agent_contracts::CONTEXT_MANAGE.into(),
                        ok: true,
                        summary: "hit".into(),
                        model_content: "hit".into(),
                        artifact_ref: None,
                        metadata: json!({
                            "op": "search",
                            "descriptors": [{
                                "ref": { "id": forgotten.to_string() }
                            }]
                        }),
                    },
                },
            ),
        ];
        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.forgotten_items, 1);
        assert_eq!(metrics.recovered_items, 1);
        assert_eq!(metrics.recovery_auto_reactivation, 1);
        assert_eq!(metrics.recovery_explicit_search, 0);
        assert_eq!(metrics.recovery_failed, 0);
        assert_eq!(metrics.reactivation_events, 1);
        assert_eq!(metrics.unique_reactivated, 1);
    }

    #[test]
    fn rejected_user_input_is_not_a_turn() {
        let run = RunId::new();
        let rejected =
            RuntimeInputEnvelope::from_preview("second").with_lifecycle(InputLifecycle::Rejected);
        let events = vec![
            envelope(run, 1, RuntimeEvent::user_message_accepted("first")),
            envelope(
                run,
                2,
                RuntimeEvent::UserMessageAccepted { input: rejected },
            ),
        ];
        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.turns, 1);
    }

    #[test]
    fn compaction_pass_cost_survives_a_later_zero_gc_snapshot() {
        let run = RunId::new();
        let events = vec![
            envelope(
                run,
                1,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::AfterModel,
                    report: ContextMaintenanceReport {
                        compaction_input_tokens: 80,
                        compaction_output_tokens: 20,
                        diagnostics: ContextDiagnostics {
                            compaction_input_tokens: 80,
                            compaction_output_tokens: 20,
                            ..ContextDiagnostics::default()
                        },
                        ..ContextMaintenanceReport::default()
                    },
                },
            ),
            envelope(
                run,
                2,
                RuntimeEvent::ContextGc {
                    report: ContextGcReport {
                        diagnostics: ContextDiagnostics::default(),
                        ..ContextGcReport::default()
                    },
                },
            ),
        ];
        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.compaction_input_tokens, 80);
        assert_eq!(metrics.compaction_output_tokens, 20);
    }

    #[test]
    fn context_compacted_events_are_the_cost_authority() {
        let run = RunId::new();
        let events = vec![
            envelope(
                run,
                1,
                RuntimeEvent::ContextMaintained {
                    trigger: ContextMaintenanceTrigger::UserInput,
                    report: ContextMaintenanceReport {
                        compaction_input_tokens: 80,
                        compaction_output_tokens: 20,
                        ..ContextMaintenanceReport::default()
                    },
                },
            ),
            envelope(
                run,
                2,
                RuntimeEvent::ContextCompacted {
                    reason: CompactionReason::EpisodeRotation,
                    input_tokens: 34600,
                    output_tokens: 1300,
                    source_items: 8,
                },
            ),
        ];
        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.compaction_input_tokens, 34600);
        assert_eq!(metrics.compaction_output_tokens, 1300);
    }

    #[test]
    fn materialize_percentiles_use_nearest_rank() {
        // Four samples [10, 20, 30, 40]: p50 = round(1.5) -> index 2 (30),
        // p95 = round(2.85) -> index 3 (40).
        let sorted = vec![10u64, 20, 30, 40];
        assert_eq!(percentile(&sorted, 50), 30);
        assert_eq!(percentile(&sorted, 95), 40);

        // A single sample is both p50 and p95.
        assert_eq!(percentile(&[7u64], 50), 7);
        assert_eq!(percentile(&[7u64], 95), 7);
    }

    #[test]
    fn fs_read_motive_is_counted_from_tool_finished_metadata() {
        let run = RunId::new();
        let events = vec![envelope(
            run,
            1,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: "r1".into(),
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "read".into(),
                    model_content: "body".into(),
                    artifact_ref: None,
                    metadata: json!({
                        "path": "src/util.py",
                        FS_READ_MOTIVE_KEY: "checked-fresh",
                    }),
                },
            },
        )];
        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.reread_motive_checked_fresh, 1);
        assert_eq!(metrics.reread_motive_first, 0);
        assert!(render_metrics(&metrics).contains("checked_fresh=1"));
    }

    #[test]
    fn legacy_selected_current_motive_counts_as_body_visible() {
        let run = RunId::new();
        let events = vec![envelope(
            run,
            1,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: "r1".into(),
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "read".into(),
                    model_content: "body".into(),
                    artifact_ref: None,
                    metadata: json!({
                        "path": "src/util.py",
                        FS_READ_MOTIVE_KEY: "selected-current",
                    }),
                },
            },
        )];
        let metrics = aggregate_metrics(&events);
        assert_eq!(metrics.reread_motive_body_visible_current, 1);
        assert_eq!(metrics.reread_motive_descriptor_only, 0);
    }

    #[test]
    fn protocol_checkpoint_body_missing_motive_is_counted() {
        let run = RunId::new();
        let events = vec![envelope(
            run,
            1,
            RuntimeEvent::ToolFinished {
                output: ToolOutput {
                    call_id: "r1".into(),
                    tool_name: "fs.read".into(),
                    ok: true,
                    summary: "read".into(),
                    model_content: "body".into(),
                    artifact_ref: None,
                    metadata: json!({
                        "path": "src/util.py",
                        FS_READ_MOTIVE_KEY: "protocol-checkpoint-body-missing",
                    }),
                },
            },
        )];
        let metrics = aggregate_metrics(&events);
        assert_eq!(
            metrics.reread_motive_protocol_checkpoint_body_missing,
            1
        );
        assert!(render_metrics(&metrics).contains("protocol_checkpoint_body_missing=1"));
    }
}
