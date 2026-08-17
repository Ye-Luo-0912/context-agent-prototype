//! 每个 intended cell 一份有界证据包（EVAL-01）：事件 JSONL、机器可读
//! 摘要、workspace 哈希、可重放的 hidden 断言（文件体 + 谓词结果）。
//! 报告表从这些包重建，不靠已经删掉的临时目录。不含 API key。

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use agent_contracts::{RuntimeEvent, RuntimeEventEnvelope};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::metrics::RunMetrics;
use crate::workload::{self, CodingFixture, HiddenReport};

/// 证据包 schema。升版本时旧包仍可读，新字段要有 default。
/// `ANALYSIS_SCHEMA` 跟着预注册 spec 走；旧 `pair.json` 里的 v1 仍能加载。
pub const CELL_SCHEMA: &str = "agent-eval.cell.v1";
pub const PAIR_SCHEMA: &str = "agent-eval.pair.v1";
pub const ANALYSIS_SCHEMA: &str = "agent-eval.analysis.v2";

/// 工作区文件清单上限：哈希覆盖全部，清单只留前 N 条路径。
const WORKSPACE_LIST_CAP: usize = 256;

/// 一次配对（同一 fixture × 同一 repeat 的 A/B/C）。
#[derive(Debug, Clone)]
pub struct PairSink {
    pub root: PathBuf,
    pub fixture_id: String,
    pub repeat: u32,
    pub repeats: u32,
    pub live: bool,
}

impl PairSink {
    pub fn cell_dir(&self, engine: &str) -> PathBuf {
        self.root
            .join(&self.fixture_id)
            .join(format!("r{}", self.repeat))
            .join(engine)
    }
}

/// 写入时的运行身份。模型名/基址来自环境，从不写 API key。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellManifest {
    pub schema: String,
    pub fixture_id: String,
    pub engine: String,
    pub repeat: u32,
    pub repeats: u32,
    pub live: bool,
    pub fixture_sha256: String,
    pub git_head: Option<String>,
    pub git_dirty: Option<bool>,
    /// `git status --porcelain` 的 sha256；干净树是空输入的哈希。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dirty_sha256: Option<String>,
    pub openai_model: Option<String>,
    pub openai_base_url: Option<String>,
}

/// 从事件流抽出的工具直方图，用来解释 live 回合膨胀（空 search、
/// 重复 read、失败调用），不必先读完整 JSONL。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolCount {
    pub name: String,
    pub calls: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellSummary {
    pub schema: String,
    pub outcome: String,
    pub error: Option<String>,
    pub passed: bool,
    pub wall_ms: u64,
    pub seq_contiguous: bool,
    pub seq_gap: Option<(u64, u64)>,
    pub broadcast_lagged: u64,
    pub model_deltas_omitted: u64,
    pub model_started: u64,
    pub model_used: u64,
    /// `ModelStarted` 多于 `ModelUsed`，或有 round 但用量全是 0：不能把
    /// 缺 usage 当成测得的零。
    pub usage_incomplete: bool,
    pub workspace_sha256: String,
    pub workspace_files: usize,
    pub tools: Vec<ToolCount>,
    pub metrics: serde_json::Value,
}

/// 一次 cell 跑完（或中途失败）后写入证据包。失败也要落盘，否则超时
/// 细胞从配对里消失，ITT 无法重建。
#[allow(clippy::too_many_arguments)]
pub fn write_cell(
    dir: &Path,
    fixture: &CodingFixture,
    engine: &str,
    pair: &PairSink,
    events: &[RuntimeEventEnvelope],
    metrics: &RunMetrics,
    passed: bool,
    wall_ms: u64,
    error: Option<&str>,
    workspace_root: &Path,
    broadcast_lagged: u64,
    model_deltas_omitted: u64,
) -> anyhow::Result<()> {
    let report = workload::evaluate_hidden(fixture, workspace_root);
    write_cell_parts(
        dir,
        fixture.id,
        &fixture_sha256(fixture),
        engine,
        pair,
        events,
        metrics,
        passed,
        wall_ms,
        error,
        workspace_root,
        broadcast_lagged,
        model_deltas_omitted,
        &report,
    )
}

/// 套件 live 细胞：hidden 是可执行命令，不是烟雾 fixture 的文件体断言。
#[allow(clippy::too_many_arguments)]
pub fn write_suite_cell(
    dir: &Path,
    task: &crate::suite::SuiteTask,
    engine: &str,
    pair: &PairSink,
    events: &[RuntimeEventEnvelope],
    metrics: &RunMetrics,
    passed: bool,
    wall_ms: u64,
    error: Option<&str>,
    workspace_root: &Path,
    broadcast_lagged: u64,
    model_deltas_omitted: u64,
    commands: Vec<crate::workload::HiddenCommandResult>,
) -> anyhow::Result<()> {
    let command_passed = crate::suite::all_hidden_passed(&commands);
    let report = HiddenReport {
        schema: crate::workload::VERIFY_SCHEMA.to_string(),
        kind: "hidden_command".into(),
        fixture_id: task.id.clone(),
        expected_edit: String::new(),
        passed: command_passed,
        replay_complete: true,
        assertions: Vec::new(),
        files: Vec::new(),
        commands,
    };
    write_cell_parts(
        dir,
        &task.id,
        &suite_task_sha256(task),
        engine,
        pair,
        events,
        metrics,
        passed,
        wall_ms,
        error,
        workspace_root,
        broadcast_lagged,
        model_deltas_omitted,
        &report,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_cell_parts(
    dir: &Path,
    fixture_id: &str,
    fixture_sha256: &str,
    engine: &str,
    pair: &PairSink,
    events: &[RuntimeEventEnvelope],
    metrics: &RunMetrics,
    passed: bool,
    wall_ms: u64,
    error: Option<&str>,
    workspace_root: &Path,
    broadcast_lagged: u64,
    model_deltas_omitted: u64,
    report: &HiddenReport,
) -> anyhow::Result<()> {
    fs::create_dir_all(dir)?;
    let manifest = CellManifest {
        schema: CELL_SCHEMA.to_string(),
        fixture_id: fixture_id.to_string(),
        engine: engine.to_string(),
        repeat: pair.repeat,
        repeats: pair.repeats,
        live: pair.live,
        fixture_sha256: fixture_sha256.to_string(),
        git_head: git_head(),
        git_dirty: git_dirty(),
        git_dirty_sha256: git_dirty_sha256(),
        openai_model: crate::envfile::get("OPENAI_MODEL"),
        openai_base_url: crate::envfile::get("OPENAI_BASE_URL"),
    };
    write_json(dir.join("manifest.json"), &manifest)?;

    let mut jsonl = fs::File::create(dir.join("events.jsonl"))?;
    for envelope in events {
        serde_json::to_writer(&mut jsonl, envelope)?;
        jsonl.write_all(b"\n")?;
    }

    let (workspace_sha256, files) = hash_workspace(workspace_root)?;
    let file_list: Vec<_> = files.iter().take(WORKSPACE_LIST_CAP).collect();
    write_json(
        dir.join("workspace.json"),
        &json!({
            "sha256": workspace_sha256,
            "files": files.len(),
            "listed": file_list,
        }),
    )?;
    write_json(dir.join("verify.json"), report)?;

    let seq_gap = first_journaled_seq_gap(events);
    let model_started = count_event(events, |e| matches!(e, RuntimeEvent::ModelStarted { .. }));
    let model_used = count_event(events, |e| matches!(e, RuntimeEvent::ModelUsed { .. }));
    let usage_incomplete = model_started > model_used
        || (model_started > 0
            && metrics.model_input_tokens == 0
            && metrics.model_output_tokens == 0);
    let outcome = if error.is_some() {
        "error"
    } else if passed {
        "passed"
    } else {
        "verify_failed"
    };
    let summary = CellSummary {
        schema: CELL_SCHEMA.to_string(),
        outcome: outcome.to_string(),
        error: error.map(str::to_string),
        passed,
        wall_ms,
        seq_contiguous: seq_gap.is_none(),
        seq_gap,
        broadcast_lagged,
        model_deltas_omitted,
        model_started,
        model_used,
        usage_incomplete,
        workspace_sha256,
        workspace_files: files.len(),
        tools: tool_histogram(events),
        metrics: metrics_json(metrics),
    };
    write_json(dir.join("summary.json"), &summary)?;
    Ok(())
}

pub fn write_pair(pair: &PairSink, engines: &[&str]) -> anyhow::Result<PathBuf> {
    write_pair_with_schema(pair, engines, ANALYSIS_SCHEMA)
}

pub fn write_pair_with_schema(
    pair: &PairSink,
    engines: &[&str],
    analysis_schema: &str,
) -> anyhow::Result<PathBuf> {
    write_pair_doc(pair, engines, analysis_schema, &json!({}))
}

pub fn write_pair_doc(
    pair: &PairSink,
    engines: &[&str],
    analysis_schema: &str,
    extra: &serde_json::Value,
) -> anyhow::Result<PathBuf> {
    let dir = pair
        .root
        .join(&pair.fixture_id)
        .join(format!("r{}", pair.repeat));
    fs::create_dir_all(&dir)?;
    let cells: Vec<_> = engines
        .iter()
        .map(|engine| {
            json!({
                "engine": engine,
                "dir": engine,
            })
        })
        .collect();
    let mut doc = json!({
        "schema": PAIR_SCHEMA,
        "fixture_id": pair.fixture_id,
        "repeat": pair.repeat,
        "repeats": pair.repeats,
        "live": pair.live,
        "arm_order": engines,
        "analysis_schema": analysis_schema,
        "cells": cells,
    });
    if let Some(map) = extra.as_object() {
        for (key, value) in map {
            doc[key] = value.clone();
        }
    }
    write_json(dir.join("pair.json"), &doc)?;
    Ok(dir)
}

/// 从 cell 目录或 pair.json 所在目录打印可读表。
pub fn render_evidence(path: &Path) -> anyhow::Result<String> {
    let pair_path = if path.join("pair.json").is_file() {
        path.join("pair.json")
    } else if path.file_name().is_some_and(|name| name == "pair.json") {
        path.to_path_buf()
    } else {
        return render_cell(path);
    };
    let pair: serde_json::Value = serde_json::from_str(&fs::read_to_string(&pair_path)?)?;
    let mut out = String::new();
    out.push_str(&format!(
        "pair fixture={} repeat={}/{}\n",
        pair.get("fixture_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?"),
        pair.get("repeat").and_then(|v| v.as_u64()).unwrap_or(0),
        pair.get("repeats").and_then(|v| v.as_u64()).unwrap_or(0)
    ));
    let pair_dir = pair_path.parent().unwrap_or(path);
    if let Some(cells) = pair.get("cells").and_then(|v| v.as_array()) {
        for cell in cells {
            let dir = cell
                .get("dir")
                .and_then(|v| v.as_str())
                .map(|s| {
                    let p = PathBuf::from(s);
                    if p.is_absolute() { p } else { pair_dir.join(s) }
                })
                .unwrap_or_default();
            out.push_str(&render_cell(&dir)?);
        }
    }
    if pair.get("analysis_schema").and_then(|v| v.as_str()) == Some(crate::context_bench::SCHEMA) {
        out.push('\n');
        out.push_str(&crate::context_bench::render_why_from_pair(pair_dir)?);
    }
    Ok(out)
}

fn render_cell(dir: &Path) -> anyhow::Result<String> {
    let summary_path = if dir.join("summary.json").is_file() {
        dir.join("summary.json")
    } else {
        dir.to_path_buf()
    };
    let parent = summary_path.parent().unwrap_or(dir);
    let summary: CellSummary = serde_json::from_str(&fs::read_to_string(&summary_path)?)?;
    let manifest: CellManifest =
        serde_json::from_str(&fs::read_to_string(parent.join("manifest.json"))?)?;
    let mut out = String::new();
    out.push_str(&format!(
        "  {:8} outcome={} passed={} wall_ms={} rounds={} tools={} search={}/{} empty={} p50_ms={} lagged={} usage_incomplete={}\n",
        manifest.engine,
        summary.outcome,
        summary.passed,
        summary.wall_ms,
        summary.metrics.get("rounds").and_then(|v| v.as_u64()).unwrap_or(0),
        summary.metrics.get("tool_calls").and_then(|v| v.as_u64()).unwrap_or(0),
        summary.metrics.get("search_calls").and_then(|v| v.as_u64()).unwrap_or(0),
        summary.metrics.get("search_hits").and_then(|v| v.as_u64()).unwrap_or(0),
        summary.metrics.get("search_empty").and_then(|v| v.as_u64()).unwrap_or(0),
        summary.metrics.get("search_ms_p50").and_then(|v| v.as_u64()).unwrap_or(0),
        summary.broadcast_lagged,
        summary.usage_incomplete,
    ));
    out.push_str(&format!(
        "           forgotten={} recovered={} search={} reactivate={} reread={} failed={} compact={}/{}\n",
        summary
            .metrics
            .get("forgotten_items")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        summary
            .metrics
            .get("recovered_items")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        summary
            .metrics
            .get("recovery_explicit_search")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        summary
            .metrics
            .get("recovery_auto_reactivation")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        summary
            .metrics
            .get("repeated_fs_reads")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        summary
            .metrics
            .get("failed_tool_outputs")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        summary
            .metrics
            .get("compaction_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        summary
            .metrics
            .get("compaction_output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    ));
    if let Some(error) = &summary.error {
        out.push_str(&format!("           error={error}\n"));
    }
    if parent.join("verify.json").is_file()
        && let Ok(text) = fs::read_to_string(parent.join("verify.json"))
        && let Ok(report) = serde_json::from_str::<HiddenReport>(&text)
    {
        out.push_str(&format!(
            "           hidden kind={} passed={} replay_complete={} asserts={}/{}\n",
            report.kind,
            report.passed,
            report.replay_complete,
            report.assertions.iter().filter(|row| row.passed).count(),
            report.assertions.len()
        ));
        match crate::workload::reverify_from_report(&report) {
            Ok(replayed) if replayed != report.passed => {
                out.push_str(&format!(
                    "           hidden reverify mismatch stored={} replayed={replayed}\n",
                    report.passed
                ));
            }
            Err(error) => {
                out.push_str(&format!("           hidden reverify error={error}\n"));
            }
            _ => {}
        }
        for row in &report.assertions {
            if !row.passed {
                out.push_str(&format!(
                    "           hidden FAIL {} {} {:?}\n",
                    row.path, row.pred, row.needles
                ));
            }
        }
        for row in &report.commands {
            if !row.passed {
                out.push_str(&format!("           hidden CMD FAIL {:?}\n", row.argv));
            }
        }
    }
    for tool in &summary.tools {
        out.push_str(&format!(
            "           tool {} calls={} failed={}\n",
            tool.name, tool.calls, tool.failed
        ));
    }
    Ok(out)
}

pub fn fixture_sha256(fixture: &CodingFixture) -> String {
    let mut hasher = Sha256::new();
    hasher.update(fixture.id.as_bytes());
    hasher.update(b"\n");
    hasher.update(fixture.description.as_bytes());
    hasher.update(b"\n");
    for turn in fixture.extra_live_turns {
        hasher.update(turn.as_bytes());
        hasher.update(b"\n");
    }
    for (path, content) in fixture.seed {
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
        hasher.update(content.as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(fixture.expected_edit.as_bytes());
    crate::workload::hash_hidden(fixture, &mut hasher);
    hex_encode(hasher.finalize())
}

pub fn suite_task_sha256(task: &crate::suite::SuiteTask) -> String {
    let mut hasher = Sha256::new();
    hasher.update(task.id.as_bytes());
    hasher.update(b"\n");
    hasher.update(task.description.as_bytes());
    hasher.update(b"\n");
    for turn in &task.extra_live_turns {
        hasher.update(turn.as_bytes());
        hasher.update(b"\n");
    }
    for file in &task.seed {
        hasher.update(file.path.as_bytes());
        hasher.update(b"\n");
        hasher.update(file.content.as_bytes());
        hasher.update(b"\n");
    }
    for cmd in &task.hidden_commands {
        for arg in &cmd.argv {
            hasher.update(arg.as_bytes());
            hasher.update(b"\0");
        }
        hasher.update(b"\n");
    }
    hex_encode(hasher.finalize())
}

/// 广播流里的 `ModelDelta` 不占新的 durable seq；检查时跳过它们。
pub fn first_journaled_seq_gap(events: &[RuntimeEventEnvelope]) -> Option<(u64, u64)> {
    let mut expected = 1u64;
    for envelope in events {
        if matches!(envelope.event, RuntimeEvent::ModelDelta { .. }) {
            continue;
        }
        if envelope.seq != expected {
            return Some((expected, envelope.seq));
        }
        expected = expected.saturating_add(1);
    }
    None
}

pub fn tool_histogram(events: &[RuntimeEventEnvelope]) -> Vec<ToolCount> {
    let mut started: BTreeMap<String, u64> = BTreeMap::new();
    let mut failed: BTreeMap<String, u64> = BTreeMap::new();
    for envelope in events {
        match &envelope.event {
            RuntimeEvent::ToolStarted { call } => {
                *started.entry(call.name.clone()).or_default() += 1;
            }
            RuntimeEvent::ToolFinished { output } if !output.ok => {
                *failed.entry(output.tool_name.clone()).or_default() += 1;
            }
            _ => {}
        }
    }
    started
        .into_iter()
        .map(|(name, calls)| ToolCount {
            failed: failed.get(&name).copied().unwrap_or(0),
            name,
            calls,
        })
        .collect()
}

fn hash_workspace(root: &Path) -> anyhow::Result<(String, Vec<String>)> {
    let mut files = Vec::new();
    collect_files(root, root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for rel in &files {
        hasher.update(rel.as_bytes());
        hasher.update(b"\0");
        let bytes = fs::read(root.join(rel))?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
        hasher.update(b"\n");
    }
    Ok((hex_encode(hasher.finalize()), files))
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name == ".focus-agent" {
            continue;
        }
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(rel);
        }
    }
    Ok(())
}

fn metrics_json(metrics: &RunMetrics) -> serde_json::Value {
    json!({
        "model_input_tokens": metrics.model_input_tokens,
        "model_output_tokens": metrics.model_output_tokens,
        "schema_tokens_total": metrics.schema_tokens_total,
        "rounds": metrics.rounds,
        "turns": metrics.turns,
        "tool_calls": metrics.tool_calls,
        "lifecycle_transitions": metrics.lifecycle_transitions,
        "failed_tool_outputs": metrics.failed_tool_outputs,
        "tool_failure_classes": metrics.tool_failure_classes,
        "repeated_fs_reads": metrics.repeated_fs_reads,
        "search_calls": metrics.search_calls,
        "search_hits": metrics.search_hits,
        "search_empty": metrics.search_empty,
        "search_ms_p50": metrics.search_ms_p50,
        "search_ms_p95": metrics.search_ms_p95,
        "inspect_calls": metrics.inspect_calls,
        "fetch_calls": metrics.fetch_calls,
        "admit_calls": metrics.admit_calls,
        "recovered_items": metrics.recovered_items,
        "forgotten_items": metrics.forgotten_items,
        "recovery_explicit_search": metrics.recovery_explicit_search,
        "recovery_auto_reactivation": metrics.recovery_auto_reactivation,
        "recovery_workspace_reread": metrics.recovery_workspace_reread,
        "recovery_failed": metrics.recovery_failed,
        "access_search_hits": metrics.access_search_hits,
        "access_inspects": metrics.access_inspects,
        "access_fetches": metrics.access_fetches,
        "access_admits": metrics.access_admits,
        "access_consumption_acks": metrics.access_consumption_acks,
        "reactivation_selected": metrics.reactivation_selected,
        "reactivation_consumed": metrics.reactivation_consumed,
        "reactivation_selected_tokens": metrics.reactivation_selected_tokens,
        "reactivation_consumed_tokens": metrics.reactivation_consumed_tokens,
        "compaction_input_tokens": metrics.compaction_input_tokens,
        "compaction_output_tokens": metrics.compaction_output_tokens,
        "provider_tokens_total": metrics
            .model_input_tokens
            .saturating_add(metrics.model_output_tokens)
            .saturating_add(metrics.compaction_input_tokens)
            .saturating_add(metrics.compaction_output_tokens),
        "final_resident_bytes": metrics.final_resident_bytes,
        "peak_resident_bytes": metrics.peak_resident_bytes,
        "materialize_rounds": metrics.materialize_rounds,
    })
}

fn count_event(events: &[RuntimeEventEnvelope], pred: impl Fn(&RuntimeEvent) -> bool) -> u64 {
    events
        .iter()
        .filter(|envelope| pred(&envelope.event))
        .count() as u64
}

fn write_json(path: PathBuf, value: &impl Serialize) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    fs::write(&path, text)?;
    Ok(())
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn git_head() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let head = String::from_utf8(output.stdout).ok()?;
    let head = head.trim();
    if head.is_empty() {
        None
    } else {
        Some(head.to_string())
    }
}

fn git_porcelain() -> Option<Vec<u8>> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

fn git_dirty() -> Option<bool> {
    git_porcelain().map(|bytes| !bytes.is_empty())
}

fn git_dirty_sha256() -> Option<String> {
    let bytes = git_porcelain()?;
    let digest = Sha256::digest(&bytes);
    Some(hex_encode(digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{RunId, ToolCall, ToolOutput};
    use serde_json::json;

    fn envelope(seq: u64, event: RuntimeEvent) -> RuntimeEventEnvelope {
        RuntimeEventEnvelope {
            run_id: RunId::new(),
            seq,
            timestamp_ms: seq,
            event,
        }
    }

    #[test]
    fn journaled_seq_skips_model_delta_repeats() {
        let events = vec![
            envelope(1, RuntimeEvent::RunStarted),
            envelope(
                1,
                RuntimeEvent::ModelDelta {
                    turn_id: Default::default(),
                    operation_id: Default::default(),
                    generation: 0,
                    delta: "x".into(),
                },
            ),
            envelope(2, RuntimeEvent::TurnCompleted),
        ];
        assert_eq!(first_journaled_seq_gap(&events), None);
        let gapped = vec![
            envelope(1, RuntimeEvent::RunStarted),
            envelope(3, RuntimeEvent::TurnCompleted),
        ];
        assert_eq!(first_journaled_seq_gap(&gapped), Some((2, 3)));
    }

    #[test]
    fn writes_a_rebuildable_cell_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(workspace.join("src")).unwrap();
        fs::write(workspace.join("src/util.py"), "ok\n").unwrap();
        let fixture = &crate::workload::FIXTURES[0];
        let pair = PairSink {
            root: tmp.path().join("evidence"),
            fixture_id: fixture.id.to_string(),
            repeat: 1,
            repeats: 1,
            live: false,
        };
        let events = vec![
            envelope(1, RuntimeEvent::RunStarted),
            envelope(
                2,
                RuntimeEvent::ToolStarted {
                    call: ToolCall {
                        id: "c1".into(),
                        name: "fs.read".into(),
                        arguments: json!({"path": "src/util.py"}),
                    },
                },
            ),
            envelope(
                3,
                RuntimeEvent::ToolFinished {
                    output: ToolOutput {
                        call_id: "c1".into(),
                        tool_name: "fs.read".into(),
                        ok: true,
                        summary: "ok".into(),
                        model_content: "ok".into(),
                        artifact_ref: None,
                        metadata: json!({}),
                    },
                },
            ),
            envelope(4, RuntimeEvent::TurnCompleted),
        ];
        let metrics = crate::metrics::aggregate_metrics(&events);
        let cell = pair.cell_dir("dynamic");
        write_cell(
            &cell, fixture, "dynamic", &pair, &events, &metrics, true, 12, None, &workspace, 0, 0,
        )
        .unwrap();
        assert!(cell.join("events.jsonl").is_file());
        assert!(cell.join("summary.json").is_file());
        assert!(cell.join("manifest.json").is_file());
        let summary: CellSummary =
            serde_json::from_str(&fs::read_to_string(cell.join("summary.json")).unwrap()).unwrap();
        assert!(summary.seq_contiguous);
        assert!(summary.passed);
        assert_eq!(summary.tools.len(), 1);
        assert_eq!(summary.tools[0].name, "fs.read");
        let shown = render_cell(&cell).unwrap();
        assert!(shown.contains("fs.read"), "{shown}");
        assert!(shown.contains("compact=0/0"), "{shown}");
        let report: crate::workload::HiddenReport =
            serde_json::from_str(&fs::read_to_string(cell.join("verify.json")).unwrap()).unwrap();
        assert_eq!(report.schema, crate::workload::VERIFY_SCHEMA);
        assert_eq!(report.kind, "file_content");
        assert!(!report.assertions.is_empty());
        assert!(report.replay_complete);
        // 工作区只有 ok\n，hidden 应失败并点名 src/util.py。
        assert!(!report.passed);
        assert!(
            report
                .assertions
                .iter()
                .any(|row| row.path == "src/util.py" && !row.passed),
            "{:?}",
            report.assertions
        );
        assert!(!crate::workload::reverify_from_report(&report).unwrap());
        std::fs::remove_dir_all(&workspace).unwrap();
        assert!(!crate::workload::reverify_from_report(&report).unwrap());
        assert!(shown.contains("hidden FAIL"), "{shown}");
    }

    #[test]
    fn writes_a_hidden_command_suite_cell() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join("seed.py"), "x = 1\n").unwrap();
        let pack = crate::suite::load_pack().unwrap();
        let task = pack
            .tasks
            .iter()
            .find(|task| task.id == "python-itertools-batched")
            .expect("file task");
        let pair = PairSink {
            root: tmp.path().join("evidence"),
            fixture_id: task.id.clone(),
            repeat: 1,
            repeats: 1,
            live: true,
        };
        let commands = vec![crate::workload::HiddenCommandResult {
            argv: vec!["python".into(), "-m".into(), "unittest".into()],
            expect_exit: 0,
            exit: Some(0),
            passed: true,
            ..crate::workload::HiddenCommandResult::default()
        }];
        let cell = pair.cell_dir("append");
        write_suite_cell(
            &cell,
            task,
            "append",
            &pair,
            &[],
            &crate::metrics::RunMetrics::default(),
            true,
            3,
            None,
            &workspace,
            0,
            0,
            commands,
        )
        .unwrap();
        let report: crate::workload::HiddenReport =
            serde_json::from_str(&fs::read_to_string(cell.join("verify.json")).unwrap()).unwrap();
        assert_eq!(report.kind, "hidden_command");
        assert!(report.passed);
        assert!(crate::workload::reverify_from_report(&report).unwrap());
        let summary: CellSummary =
            serde_json::from_str(&fs::read_to_string(cell.join("summary.json")).unwrap()).unwrap();
        assert_eq!(summary.outcome, "passed");
    }
}
