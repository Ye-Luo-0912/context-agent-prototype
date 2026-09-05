//! Frame-1 offline comparison: aggregate shadow Context Frame manifests
//! (`ContextFrameShadow` events) against the actual context-layer token
//! costs recorded per model round (`ModelStarted.prompt_layers`), from one
//! or more trace JSONL files. The trace must come from a run with the
//! `shadow_context_frame` flag enabled; rounds without a manifest are
//! counted as uncovered so a mixed capture is visible instead of silently
//! averaged away.

use serde_json::Value;

/// One paired model round: actual context-layer cost vs the structured
/// frame the compiler would have produced from the same state.
#[derive(Debug, Clone)]
pub struct FrameRoundComparison {
    pub run_id: String,
    pub round: usize,
    /// `historical_context_tokens` (+ restored protocol bodies) actually
    /// sent in the context layer this round.
    pub actual_context_tokens: u64,
    /// `approx_tokens_total` of the shadow frame manifest.
    pub shadow_frame_tokens: usize,
    pub duplicates_removed: usize,
    pub required_misses: usize,
    pub body_blocks: usize,
    pub descriptor_blocks: usize,
    pub zones: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Default)]
pub struct FrameComparisonReport {
    pub runs: usize,
    /// Model rounds observed in the traces.
    pub rounds: usize,
    /// Rounds paired with a shadow manifest (flag enabled for them).
    pub rounds_with_shadow: usize,
    pub total_actual_context_tokens: u64,
    pub total_shadow_frame_tokens: usize,
    pub total_duplicates_removed: usize,
    pub rounds_with_required_misses: usize,
    pub body_blocks: usize,
    pub descriptor_blocks: usize,
    /// `zone name -> summed approx_tokens` across all paired rounds.
    pub zone_tokens: Vec<(String, usize)>,
    pub per_round: Vec<FrameRoundComparison>,
}

/// Parse one trace JSONL stream and fold its rounds into `report`.
pub fn fold_frame_trace(lines: impl Iterator<Item = String>, report: &mut FrameComparisonReport) {
    let mut current_run: Option<String> = None;
    let mut round = 0usize;
    // The shadow manifest is emitted during round preparation, before the
    // model operation starts: keep the latest one and pair it with the next
    // `model_started`.
    let mut pending_shadow: Option<Value> = None;

    for line in lines {
        let Ok(envelope) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let run_id = envelope
            .get("run_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        if current_run.as_deref() != Some(run_id.as_str()) {
            if current_run.is_some() {
                report.runs += 1;
            }
            current_run = Some(run_id.clone());
            round = 0;
            pending_shadow = None;
        }
        let Some(event) = envelope.get("event") else {
            continue;
        };
        let Some(kind) = event.get("type").and_then(|value| value.as_str()) else {
            continue;
        };
        match kind {
            "context_frame_shadow" => {
                if let Some(manifest) = event.get("manifest").and_then(|m| m.as_str())
                    && let Ok(parsed) = serde_json::from_str::<Value>(manifest)
                {
                    pending_shadow = Some(parsed);
                }
            }
            "model_started" => {
                round += 1;
                report.rounds += 1;
                let layers = event.get("prompt_layers");
                let actual = layers
                    .and_then(|layers| layers.get("historical_context_tokens"))
                    .and_then(|value| value.as_u64())
                    .unwrap_or(0)
                    + layers
                        .and_then(|layers| layers.get("restored_protocol_tokens"))
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                if let Some(manifest) = pending_shadow.take() {
                    report.rounds_with_shadow += 1;
                    let shadow = manifest
                        .get("approx_tokens_total")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0) as usize;
                    let duplicates = manifest
                        .get("duplicates_removed")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0) as usize;
                    let misses = manifest
                        .get("required_misses")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0) as usize;
                    if misses > 0 {
                        report.rounds_with_required_misses += 1;
                    }
                    let mut body_blocks = 0usize;
                    let mut descriptor_blocks = 0usize;
                    let mut zones: Vec<(String, usize)> = Vec::new();
                    if let Some(list) = manifest.get("zones").and_then(|z| z.as_array()) {
                        for zone in list {
                            let name = zone
                                .get("zone")
                                .and_then(|value| value.as_str())
                                .unwrap_or("?")
                                .to_string();
                            let tokens = zone
                                .get("approx_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0) as usize;
                            match report
                                .zone_tokens
                                .iter_mut()
                                .find(|(seen, _)| seen == &name)
                            {
                                Some(entry) => entry.1 += tokens,
                                None => report.zone_tokens.push((name.clone(), tokens)),
                            }
                            zones.push((name, tokens));
                        }
                    }
                    if let Some(blocks) = manifest.get("blocks").and_then(|b| b.as_array()) {
                        for block in blocks {
                            match block.get("representation").and_then(|value| value.as_str()) {
                                Some("descriptor") | Some("omitted") => descriptor_blocks += 1,
                                Some("bounded_body") => body_blocks += 1,
                                _ => {}
                            }
                        }
                    }
                    report.body_blocks += body_blocks;
                    report.descriptor_blocks += descriptor_blocks;
                    report.total_duplicates_removed += duplicates;
                    report.total_actual_context_tokens += actual;
                    report.total_shadow_frame_tokens += shadow;
                    report.per_round.push(FrameRoundComparison {
                        run_id: run_id.clone(),
                        round,
                        actual_context_tokens: actual,
                        shadow_frame_tokens: shadow,
                        duplicates_removed: duplicates,
                        required_misses: misses,
                        body_blocks,
                        descriptor_blocks,
                        zones,
                    });
                }
            }
            _ => {}
        }
    }
    if current_run.is_some() {
        report.runs += 1;
    }
}

/// Frame-2 trace gate: fold every recorded manifest out of the traces and
/// enforce the manifest-intrinsic invariants over each one. Returns the
/// violations with their trace/run/round coordinates.
pub fn gate_frame_traces(paths: &[std::path::PathBuf]) -> anyhow::Result<Vec<String>> {
    let mut violations = Vec::new();
    for path in paths {
        let content = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
        let mut current_run = String::new();
        let mut round = 0usize;
        let mut pending: Vec<Value> = Vec::new();
        for line in content.lines() {
            let Ok(envelope) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let run_id = envelope
                .get("run_id")
                .and_then(|value| value.as_str())
                .unwrap_or("?")
                .to_string();
            if run_id != current_run {
                current_run = run_id;
                round = 0;
                pending.clear();
            }
            let Some(event) = envelope.get("event") else {
                continue;
            };
            match event.get("type").and_then(|value| value.as_str()) {
                Some("context_frame_shadow") => {
                    if let Some(manifest) = event.get("manifest").and_then(|m| m.as_str())
                        && let Ok(parsed) = serde_json::from_str::<Value>(manifest)
                    {
                        pending.push(parsed);
                    }
                }
                Some("model_started") => {
                    round += 1;
                    let manifest = pending.pop();
                    pending.clear();
                    let Some(manifest) = manifest else {
                        continue;
                    };
                    // Recreate a FrameManifest-shaped check through the same
                    // intrinsic rules the compiler gate uses.
                    let parsed: Result<GateManifestShim, _> =
                        serde_json::from_value(manifest.clone());
                    if let Ok(parsed) = parsed {
                        for violation in intrinsic_gate(&parsed) {
                            violations
                                .push(format!("{} round {round}: {violation}", path.display()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(violations)
}

/// Local mirror of the runtime frame types so the replay binary stays
/// decoupled from agent-runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum GateZone {
    TaskContract,
    ExecutionState,
    CurrentEvidence,
    WorkingMemory,
    ExternalDirectory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum GateRepresentation {
    BoundedBody,
    Descriptor,
    Omitted,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GateManifestShim {
    zones: Vec<GateZoneStatsShim>,
    blocks: Vec<GateBlockShim>,
    required_misses: usize,
    approx_tokens_total: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GateZoneStatsShim {
    zone: GateZone,
    blocks: usize,
    omitted: usize,
    approx_tokens: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct GateBlockShim {
    zone: GateZone,
    representation: GateRepresentation,
    #[serde(default)]
    content: String,
    #[serde(default)]
    source: String,
}

fn intrinsic_gate(manifest: &GateManifestShim) -> Vec<String> {
    let mut violations = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for block in &manifest.blocks {
        if !seen.insert(&block.content) {
            violations.push(format!(
                "duplicate body in {}",
                &block.source[..12.min(block.source.len())]
            ));
        }
        if block.zone == GateZone::ExternalDirectory
            && block.representation != GateRepresentation::Descriptor
        {
            violations.push(format!(
                "external directory block is not a descriptor: {}",
                &block.source[..12.min(block.source.len())]
            ));
        }
        if block.content.chars().count() > 600 {
            violations.push(format!("unbounded body in {}", block.source));
        }
    }
    for stats in &manifest.zones {
        let zone_blocks = manifest
            .blocks
            .iter()
            .filter(|block| {
                std::mem::discriminant(&block.zone) == std::mem::discriminant(&stats.zone)
            })
            .count();
        if zone_blocks != stats.blocks {
            violations.push("zone stats disagree with blocks".to_string());
        }
    }
    violations
}

/// Aggregate one or more trace files.
pub fn frame_report_from_files(
    paths: &[std::path::PathBuf],
) -> anyhow::Result<FrameComparisonReport> {
    let mut report = FrameComparisonReport::default();
    for path in paths {
        let content = std::fs::read_to_string(path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
        fold_frame_trace(content.lines().map(|line| line.to_string()), &mut report);
    }
    Ok(report)
}

/// Compact comparison table: per-paired-round rows plus the totals.
pub fn render_frame_report(report: &FrameComparisonReport) -> String {
    let mut out = String::new();
    out.push_str("# Shadow Context Frame comparison (Frame-1)\n\n");
    out.push_str(&format!(
        "runs {} | rounds {} | paired rounds {} | rounds with required misses {}\n",
        report.runs, report.rounds, report.rounds_with_shadow, report.rounds_with_required_misses,
    ));
    out.push_str(&format!(
        "context layer tokens: actual {} vs structured frame {} ({}%)\n",
        report.total_actual_context_tokens,
        report.total_shadow_frame_tokens,
        if report.total_actual_context_tokens == 0 {
            0
        } else {
            report.total_shadow_frame_tokens * 100 / report.total_actual_context_tokens as usize
        }
    ));
    out.push_str(&format!(
        "duplicates removed {} | body blocks {} | descriptor blocks {}\n",
        report.total_duplicates_removed, report.body_blocks, report.descriptor_blocks,
    ));
    if !report.zone_tokens.is_empty() {
        out.push_str("zone tokens:");
        for (name, tokens) in &report.zone_tokens {
            out.push_str(&format!(" {name}={tokens}"));
        }
        out.push('\n');
    }
    out.push_str("\n| run | round | actual ctx | shadow | dups | misses | body | desc |\n");
    out.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for round in &report.per_round {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            &round.run_id[..8.min(round.run_id.len())],
            round.round,
            round.actual_context_tokens,
            round.shadow_frame_tokens,
            round.duplicates_removed,
            round.required_misses,
            round.body_blocks,
            round.descriptor_blocks,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(run: &str, event: &str, extra: &str) -> String {
        format!(
            r#"{{"run_id":"{run}","seq":1,"timestamp_ms":1,"event":{{"type":"{event}"{extra}}}}}"#
        )
    }

    fn manifest(tokens: usize, dups: usize, misses: usize) -> String {
        serde_json::json!({
            "schema": "context-frame-shadow/v1",
            "approx_tokens_total": tokens,
            "duplicates_removed": dups,
            "required_misses": misses,
            "zones": [
                {"zone": "task_contract", "blocks": 2, "omitted": 0, "approx_tokens": tokens / 2},
                {"zone": "working_memory", "blocks": 1, "omitted": 0, "approx_tokens": tokens / 2},
            ],
            "blocks": [
                {"zone": "task_contract", "representation": "bounded_body"},
                {"zone": "external_directory", "representation": "descriptor"},
            ],
        })
        .to_string()
    }

    #[test]
    fn pairs_shadow_manifests_with_the_following_model_round() {
        let lines = vec![
            envelope("run-a", "run_started", ""),
            envelope(
                "run-a",
                "context_frame_shadow",
                &format!(
                    r#", "manifest": {}"#,
                    serde_json::json!(manifest(100, 3, 0))
                ),
            ),
            envelope(
                "run-a",
                "model_started",
                r#", "prompt_layers": {"historical_context_tokens": 400, "restored_protocol_tokens": 25}"#,
            ),
            envelope("run-a", "turn_completed", ""),
        ];
        let mut report = FrameComparisonReport::default();
        fold_frame_trace(lines.into_iter(), &mut report);
        assert_eq!(report.runs, 1);
        assert_eq!(report.rounds, 1);
        assert_eq!(report.rounds_with_shadow, 1);
        assert_eq!(report.total_actual_context_tokens, 425);
        assert_eq!(report.total_shadow_frame_tokens, 100);
        assert_eq!(report.total_duplicates_removed, 3);
        assert_eq!(report.rounds_with_required_misses, 0);
        assert_eq!(report.body_blocks, 1);
        assert_eq!(report.descriptor_blocks, 1);
        assert!(
            report
                .zone_tokens
                .iter()
                .any(|(name, tokens)| name == "task_contract" && *tokens == 50)
        );
        assert_eq!(report.per_round.len(), 1);
    }

    #[test]
    fn uncovered_rounds_and_run_boundaries_are_visible() {
        let lines = vec![
            // run-a round 1: no manifest captured (flag off)
            envelope(
                "run-a",
                "model_started",
                r#", "prompt_layers": {"historical_context_tokens": 500}"#,
            ),
            // run-b: two covered rounds
            envelope("run-b", "run_started", ""),
            envelope(
                "run-b",
                "context_frame_shadow",
                &format!(r#", "manifest": {}"#, serde_json::json!(manifest(80, 0, 2))),
            ),
            envelope(
                "run-b",
                "model_started",
                r#", "prompt_layers": {"historical_context_tokens": 300}"#,
            ),
            envelope(
                "run-b",
                "context_frame_shadow",
                &format!(r#", "manifest": {}"#, serde_json::json!(manifest(60, 1, 0))),
            ),
            envelope(
                "run-b",
                "model_started",
                r#", "prompt_layers": {"historical_context_tokens": 310}"#,
            ),
        ];
        let mut report = FrameComparisonReport::default();
        fold_frame_trace(lines.into_iter(), &mut report);
        assert_eq!(report.runs, 2);
        assert_eq!(report.rounds, 3);
        assert_eq!(report.rounds_with_shadow, 2);
        // Totals cover paired rounds only; the uncovered round shows up as
        // rounds(3) vs rounds_with_shadow(2).
        assert_eq!(report.total_actual_context_tokens, 300 + 310);
        assert_eq!(report.total_shadow_frame_tokens, 80 + 60);
        assert_eq!(report.rounds_with_required_misses, 1);
        assert_eq!(report.per_round.len(), 2);
        let rendered = render_frame_report(&report);
        assert!(rendered.contains("runs 2"));
        assert!(rendered.contains("paired rounds 2"));
    }
}

#[cfg(test)]
mod gate_tests {
    use super::*;

    fn trace_with_manifest(manifest: serde_json::Value) -> String {
        let run = "run-gate";
        let shadow_line = format!(
            r#"{{"run_id":"{run}","seq":1,"timestamp_ms":1,"event":{{"type":"context_frame_shadow","manifest":{}}}}}"#,
            serde_json::json!(manifest.to_string())
        );
        let model_line = r#"{"run_id":"run-gate","seq":2,"timestamp_ms":2,"event":{"type":"model_started","prompt_layers":{"historical_context_tokens":100}}}"#;
        format!(
            "{shadow_line}
{model_line}"
        )
    }

    fn healthy_manifest() -> serde_json::Value {
        serde_json::json!({
            "schema": "context-frame-shadow/v1",
            "approx_tokens_total": 50,
            "duplicates_removed": 0,
            "required_misses": 0,
            "zones": [
                {"zone": "task_contract", "blocks": 1, "omitted": 0, "approx_tokens": 20},
                {"zone": "external_directory", "blocks": 1, "omitted": 0, "approx_tokens": 5},
            ],
            "blocks": [
                {"zone": "task_contract", "representation": "bounded_body",
                 "source": "anchor.original_goal", "content": "fix the thing"},
                {"zone": "external_directory", "representation": "descriptor",
                 "source": "context://run/x", "content": "externalized item"},
            ],
        })
    }

    fn write_trace(dir: &std::path::Path, line: String) -> std::path::PathBuf {
        let path = dir.join("events.jsonl");
        std::fs::write(&path, line).unwrap();
        path
    }

    #[test]
    fn the_trace_gate_passes_healthy_manifests_and_flags_violations() {
        let dir = tempfile::tempdir().unwrap();

        let healthy = write_trace(dir.path(), trace_with_manifest(healthy_manifest()));
        assert!(gate_frame_traces(&[healthy]).unwrap().is_empty());

        let mut broken = healthy_manifest();
        // Duplicate body: same content twice.
        if let Some(blocks) = broken.get_mut("blocks").and_then(|b| b.as_array_mut()) {
            let clone = blocks[0].clone();
            blocks.push(clone);
        }
        let broken_path = write_trace(dir.path(), trace_with_manifest(broken));
        let violations = gate_frame_traces(&[broken_path]).unwrap();
        assert!(
            violations.iter().any(|v| v.contains("duplicate body")),
            "{violations:?}"
        );

        let mut external_body = healthy_manifest();
        if let Some(blocks) = external_body
            .get_mut("blocks")
            .and_then(|b| b.as_array_mut())
        {
            for block in blocks.iter_mut() {
                if block["zone"] == "external_directory" {
                    block["representation"] = serde_json::json!("bounded_body");
                }
            }
        }
        let external_path = write_trace(dir.path(), trace_with_manifest(external_body));
        let violations = gate_frame_traces(&[external_path]).unwrap();
        assert!(
            violations.iter().any(|v| v.contains("not a descriptor")),
            "{violations:?}"
        );
    }
}
