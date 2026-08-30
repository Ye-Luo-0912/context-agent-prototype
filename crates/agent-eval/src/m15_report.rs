//! Mechanical M15 window reporting from immutable cell bundles.
//!
//! The live runner persists an exact cell list first. Re-rendering reads only
//! that manifest plus each cell's manifest, dimensions, and summary; it
//! validates the frozen identity and recomputes every verdict from raw facts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::bundle::{CELL_SCHEMA, CellManifest, CellSummary};
use crate::long_live::{
    AcceptanceProfile, CellFailureClass, CellVerdict, M15_PACK_IDS, PILOT_SCHEMA, PilotMode,
    evaluate_verdict,
};

const WINDOW_SCHEMA: &str = "m15-window.v1";
const FORMAL_CANDIDATE_ID: &str = "task-progress-settlement-v1";
const FORMAL_TOOL_SURFACE: &str = "production";
const REQUIRED_DIMENSION_IDENTITY_FIELDS: [&str; 16] = [
    "candidate_id",
    "source_tree_digest",
    "fixture_sha256",
    "repeat",
    "tool_surface",
    "surface_config_digest",
    "prompt_config_digest",
    "acceptance_domain",
    "acceptance_declaration_revision",
    "acceptance_source_digest",
    "provider_config_digest",
    "settlement_projection_diagnostics",
    "completion_opportunity",
    "recovery_surface",
    "project_task_progress",
    "project_settlement",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowCellRef {
    path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowManifest {
    schema: String,
    created_ms: u64,
    expected_repeats: u32,
    expected_packs: Vec<String>,
    cells: Vec<WindowCellRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Dimensions {
    schema: String,
    pack_id: String,
    candidate_id: String,
    source_tree_digest: Option<String>,
    fixture_sha256: String,
    repeat: u32,
    tool_surface: String,
    surface_config_digest: String,
    prompt_config_digest: String,
    acceptance_domain: Option<String>,
    acceptance_declaration_revision: Option<u64>,
    acceptance_source_digest: Option<String>,
    provider_config_digest: String,
    settlement_projection_diagnostics: bool,
    mode: PilotMode,
    acceptance_profile: AcceptanceProfile,
    verdict: CellVerdict,
    behavioral_oracle: String,
    observed_behavioral_oracle: String,
    allowed_diff: String,
    task_closure: String,
    continuation: String,
    exact_resume_tuple_matched: bool,
    restored: bool,
    continued: bool,
    turn_completed: bool,
    task_completed: bool,
    wall_ms: u64,
    model_rounds_phase_one: u32,
    model_rounds_phase_two: u32,
    resume_committed: u64,
    checkpoint_durable: u64,
    provider_runtime: String,
    final_passed: bool,
    runtime_error: Option<String>,
    runtime_error_class: Option<CellFailureClass>,
    #[serde(default)]
    runtime_error_retryable: bool,
    completion_opportunity: String,
    recovery_surface: String,
    project_task_progress: bool,
    project_settlement: bool,
}

#[derive(Debug)]
struct Cell {
    manifest: CellManifest,
    dimensions: Dimensions,
    summary: CellSummary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SharedIdentity {
    candidate_id: String,
    git_head: Option<String>,
    source_tree_digest: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    protocol: Option<String>,
    context_window: Option<String>,
    tool_surface: Option<String>,
    provider_config_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackIdentity {
    fixture_sha256: String,
    surface_config_digest: String,
    prompt_config_digest: String,
    acceptance_authority: AcceptanceAuthorityIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AcceptanceAuthorityIdentity {
    domain: Option<String>,
    declaration_revision: Option<u64>,
    source_digest: Option<String>,
}

pub struct RenderedWindow {
    pub markdown: String,
    pub passed: bool,
    pub censored: bool,
}

/// Persist the exact cell list for a just-finished window, then re-read that
/// list to produce the report. Existing window directories are never reused.
pub fn persist_window(
    evidence_root: &Path,
    cell_dirs: &[PathBuf],
    expected_packs: &[&str],
    expected_repeats: u32,
) -> anyhow::Result<(PathBuf, RenderedWindow)> {
    let created_ms = now_ms();
    let mut suffix = 1u32;
    let windows = evidence_root.join("_windows");
    let mut window_dir = windows.join(created_ms.to_string());
    while window_dir.exists() {
        suffix = suffix.saturating_add(1);
        window_dir = windows.join(format!("{created_ms}-{suffix}"));
    }
    std::fs::create_dir_all(&window_dir)?;

    let mut cells = Vec::with_capacity(cell_dirs.len());
    for cell_dir in cell_dirs {
        let relative = cell_dir.strip_prefix(evidence_root).with_context(|| {
            format!(
                "cell {} is outside evidence root {}",
                cell_dir.display(),
                evidence_root.display()
            )
        })?;
        let path = PathBuf::from("../..")
            .join(relative)
            .to_string_lossy()
            .replace('\\', "/");
        cells.push(WindowCellRef { path });
    }
    let manifest = WindowManifest {
        schema: WINDOW_SCHEMA.into(),
        created_ms,
        expected_repeats,
        expected_packs: expected_packs.iter().map(|id| (*id).to_string()).collect(),
        cells,
    };
    std::fs::write(
        window_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let rendered = render_window(&window_dir)?;
    std::fs::write(window_dir.join("REPORT.md"), rendered.markdown.as_bytes())?;
    Ok((window_dir, rendered))
}

pub fn render_window(window_dir: &Path) -> anyhow::Result<RenderedWindow> {
    let manifest: WindowManifest = read_json(&window_dir.join("manifest.json"))?;
    ensure!(
        manifest.schema == WINDOW_SCHEMA,
        "unsupported window schema"
    );
    ensure!(
        manifest.expected_repeats == crate::long_live::DEFAULT_REPEATS,
        "formal M15 requires exactly two repeats"
    );
    let frozen_packs: BTreeSet<&str> = M15_PACK_IDS.into_iter().collect();
    let declared_packs: BTreeSet<&str> =
        manifest.expected_packs.iter().map(String::as_str).collect();
    ensure!(
        manifest.expected_packs.len() == frozen_packs.len() && declared_packs == frozen_packs,
        "formal M15 requires the complete frozen three-pack set"
    );
    let expected_count = manifest
        .expected_packs
        .len()
        .saturating_mul(2)
        .saturating_mul(manifest.expected_repeats as usize);
    ensure!(
        manifest.cells.len() == expected_count,
        "window has {} cells, expected {expected_count}",
        manifest.cells.len()
    );

    let evidence_root = window_dir
        .parent()
        .and_then(Path::parent)
        .context("window directory must be under <evidence-root>/_windows/")?
        .canonicalize()
        .context("canonicalize evidence root")?;
    let mut cells = Vec::with_capacity(manifest.cells.len());
    for cell_ref in &manifest.cells {
        ensure!(
            !Path::new(&cell_ref.path).is_absolute(),
            "window cell paths must be relative"
        );
        let cell_dir = window_dir
            .join(&cell_ref.path)
            .canonicalize()
            .with_context(|| format!("canonicalize window cell {}", cell_ref.path))?;
        ensure!(
            cell_dir.starts_with(&evidence_root),
            "window cell escapes the evidence root"
        );
        cells.push(Cell {
            manifest: read_json(&cell_dir.join("manifest.json"))?,
            dimensions: read_dimensions(&cell_dir.join("dimensions.json"))?,
            summary: read_json(&cell_dir.join("summary.json"))?,
        });
    }
    validate_cells(&manifest, &cells)?;
    Ok(render_cells(&cells))
}

fn validate_cells(manifest: &WindowManifest, cells: &[Cell]) -> anyhow::Result<()> {
    let expected_packs: BTreeSet<&str> =
        manifest.expected_packs.iter().map(String::as_str).collect();
    let mut keys = BTreeSet::new();
    let mut shared: Option<SharedIdentity> = None;
    let mut pack_digests = BTreeMap::<&str, &str>::new();
    let mut pack_identities = BTreeMap::<&str, PackIdentity>::new();
    let mut acceptance_authorities = BTreeMap::<&str, AcceptanceAuthorityIdentity>::new();
    let expected_identities = manifest
        .expected_packs
        .iter()
        .map(|pack_id| expected_pack_identity(pack_id).map(|identity| (pack_id.as_str(), identity)))
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;

    for cell in cells {
        let dimensions = &cell.dimensions;
        let identity = &cell.manifest;
        ensure!(
            dimensions.schema == PILOT_SCHEMA,
            "legacy dimensions are forensic-only"
        );
        ensure!(
            dimensions.acceptance_profile == AcceptanceProfile::M15V1,
            "cell {} did not run the frozen M15 profile",
            dimensions.pack_id
        );
        ensure!(
            expected_packs.contains(dimensions.pack_id.as_str()),
            "unexpected pack"
        );
        let expected_identity = expected_identities
            .get(dimensions.pack_id.as_str())
            .context("missing frozen pack identity")?;
        ensure!(
            identity.fixture_id == dimensions.pack_id,
            "manifest pack mismatch"
        );
        ensure!(
            identity.schema == CELL_SCHEMA,
            "unsupported cell manifest schema"
        );
        ensure!(
            identity.engine == "dynamic",
            "formal cell used the wrong engine"
        );
        ensure!(identity.live, "formal cell is not live");
        ensure!(
            identity.git_dirty == Some(false),
            "formal cell used a dirty tree"
        );
        ensure!(
            identity.repeats == manifest.expected_repeats,
            "cell repeat plan drift"
        );
        ensure!(
            identity.repeat >= 1 && identity.repeat <= manifest.expected_repeats,
            "repeat out of range"
        );
        ensure!(
            dimensions.candidate_id == FORMAL_CANDIDATE_ID,
            "formal cell candidate identity drift"
        );
        ensure!(
            dimensions.source_tree_digest == identity.source_tree_digest,
            "dimensions/manifest source identity mismatch"
        );
        ensure!(
            dimensions.fixture_sha256 == identity.fixture_sha256,
            "dimensions/manifest fixture identity mismatch"
        );
        ensure!(
            dimensions.repeat == identity.repeat,
            "dimensions/manifest repeat identity mismatch"
        );
        ensure!(
            identity.tool_surface.as_deref() == Some(dimensions.tool_surface.as_str()),
            "dimensions/manifest tool-surface identity mismatch"
        );
        ensure!(
            dimensions.tool_surface == FORMAL_TOOL_SURFACE,
            "formal cell used the wrong tool surface"
        );
        ensure!(
            dimensions.fixture_sha256 == expected_identity.fixture_sha256,
            "frozen fixture identity drift"
        );
        ensure!(
            dimensions.surface_config_digest == expected_identity.surface_config_digest,
            "tool-surface configuration identity drift"
        );
        ensure!(
            dimensions.prompt_config_digest == expected_identity.prompt_config_digest,
            "prompt configuration identity drift"
        );
        ensure!(
            dimensions.provider_config_digest == provider_config_digest(identity)?,
            "provider configuration identity drift"
        );
        ensure!(
            dimensions.project_task_progress,
            "formal M15 requires TaskProgress projection"
        );
        ensure!(
            !dimensions.project_settlement,
            "formal M15 must keep settlement projection off"
        );
        ensure!(
            !dimensions.settlement_projection_diagnostics,
            "formal M15 must keep settlement diagnostics off"
        );
        ensure!(
            dimensions.completion_opportunity == "off" && dimensions.recovery_surface == "off",
            "formal M15 advisory switches must remain off"
        );
        validate_acceptance_authority(dimensions, &expected_identity.acceptance_authority)?;
        let acceptance = AcceptanceAuthorityIdentity {
            domain: dimensions.acceptance_domain.clone(),
            declaration_revision: dimensions.acceptance_declaration_revision,
            source_digest: dimensions.acceptance_source_digest.clone(),
        };
        match acceptance_authorities.get(dimensions.pack_id.as_str()) {
            Some(expected) => ensure!(
                *expected == acceptance,
                "acceptance authority drift within pack {}",
                dimensions.pack_id
            ),
            None => {
                acceptance_authorities.insert(&dimensions.pack_id, acceptance.clone());
            }
        }
        let observed_pack_identity = PackIdentity {
            fixture_sha256: dimensions.fixture_sha256.clone(),
            surface_config_digest: dimensions.surface_config_digest.clone(),
            prompt_config_digest: dimensions.prompt_config_digest.clone(),
            acceptance_authority: acceptance,
        };
        match pack_identities.get(dimensions.pack_id.as_str()) {
            Some(expected) => ensure!(
                *expected == observed_pack_identity,
                "pack configuration identity drift"
            ),
            None => {
                pack_identities.insert(&dimensions.pack_id, observed_pack_identity);
            }
        }
        ensure!(
            keys.insert((
                dimensions.pack_id.as_str(),
                dimensions.mode,
                identity.repeat
            )),
            "duplicate pack/mode/repeat cell"
        );

        let expected_verdict = evaluate_verdict(
            dimensions.acceptance_profile,
            dimensions.mode,
            dimensions.runtime_error_class,
            &dimensions.behavioral_oracle,
            dimensions.allowed_diff == "pass",
            dimensions.exact_resume_tuple_matched,
            dimensions.restored,
            dimensions.continued,
            dimensions.task_completed,
        );
        ensure!(
            expected_verdict == dimensions.verdict,
            "persisted verdict drift"
        );
        ensure!(
            dimensions.final_passed == (dimensions.verdict == CellVerdict::Pass),
            "final_passed disagrees with verdict"
        );
        ensure!(
            dimensions.verdict != CellVerdict::Pass
                || dimensions.turn_completed
                || dimensions.task_completed,
            "passing cell has no terminal turn/task event"
        );
        ensure!(
            dimensions.runtime_error.is_some() == dimensions.runtime_error_class.is_some(),
            "runtime error detail/class must be paired"
        );
        ensure!(
            !dimensions.runtime_error_retryable
                || dimensions.runtime_error_class == Some(CellFailureClass::ProviderTransport),
            "only provider transport failures may be retryable"
        );
        if dimensions.runtime_error_class == Some(CellFailureClass::ProviderTransport) {
            ensure!(
                dimensions.provider_runtime == "transport_failed",
                "provider class drift"
            );
        } else {
            ensure!(
                dimensions.provider_runtime == "healthy",
                "provider health drift"
            );
        }
        ensure!(
            dimensions.task_completed == (dimensions.task_closure == "completed"),
            "task closure fact drift"
        );
        ensure!(
            dimensions.observed_behavioral_oracle == "pass"
                || dimensions.observed_behavioral_oracle == "fail"
                || dimensions.observed_behavioral_oracle == "not_run",
            "invalid observed oracle state"
        );
        ensure!(
            cell.summary.schema == CELL_SCHEMA,
            "unsupported cell summary schema"
        );
        ensure!(
            cell.summary.passed == dimensions.final_passed,
            "cell summary pass flag drift"
        );
        ensure!(
            cell.summary.error == dimensions.runtime_error,
            "cell summary error drift"
        );
        ensure!(
            cell.summary.wall_ms == dimensions.wall_ms,
            "cell wall-time drift"
        );
        ensure!(
            cell.summary.seq_contiguous,
            "cell event sequence is not contiguous"
        );
        ensure!(
            cell.summary.broadcast_lagged == 0,
            "cell lost broadcast events"
        );
        ensure!(
            is_sha256(&cell.summary.workspace_sha256),
            "workspace digest is missing or malformed"
        );
        for name in [
            "rounds",
            "tool_calls",
            "model_input_tokens",
            "model_output_tokens",
            "schema_tokens_total",
        ] {
            metric_u64(&cell.summary, name)?;
        }
        ensure!(
            metric_u64(&cell.summary, "rounds")?
                == u64::from(dimensions.model_rounds_phase_one)
                    + u64::from(dimensions.model_rounds_phase_two),
            "phase-round counters disagree with event-derived metrics"
        );

        match pack_digests.get(dimensions.pack_id.as_str()) {
            Some(digest) => ensure!(**digest == identity.fixture_sha256, "pack digest drift"),
            None => {
                pack_digests.insert(&dimensions.pack_id, &identity.fixture_sha256);
            }
        }
        let current = SharedIdentity {
            candidate_id: dimensions.candidate_id.clone(),
            git_head: identity.git_head.clone(),
            source_tree_digest: identity.source_tree_digest.clone(),
            model: identity.openai_model.clone(),
            base_url: identity.openai_base_url.clone(),
            protocol: identity.openai_protocol.clone(),
            context_window: identity.openai_context_window.clone(),
            tool_surface: identity.tool_surface.clone(),
            provider_config_digest: dimensions.provider_config_digest.clone(),
        };
        ensure!(
            current.git_head.as_deref().is_some_and(non_empty)
                && current.source_tree_digest.as_deref().is_some_and(non_empty)
                && current.model.as_deref().is_some_and(non_empty)
                && current.base_url.as_deref().is_some_and(non_empty)
                && current.protocol.as_deref().is_some_and(non_empty)
                && current.context_window.as_deref().is_some_and(non_empty)
                && current.tool_surface.as_deref().is_some_and(non_empty),
            "formal cell is missing source or serving identity"
        );
        ensure!(
            current.source_tree_digest.as_deref().is_some_and(is_sha256),
            "source-tree digest is missing or malformed"
        );
        ensure!(
            !current
                .protocol
                .as_deref()
                .is_some_and(|protocol| protocol.trim().eq_ignore_ascii_case("auto")),
            "formal window cannot use auto protocol negotiation"
        );
        ensure!(
            is_sha256(&identity.fixture_sha256),
            "pack digest is malformed"
        );
        match &shared {
            Some(expected) => ensure!(*expected == current, "window identity drift"),
            None => shared = Some(current),
        }
    }
    ensure!(
        pack_digests.len() == expected_packs.len(),
        "pack coverage mismatch"
    );
    ensure!(
        pack_identities.len() == expected_packs.len()
            && acceptance_authorities.len() == expected_packs.len(),
        "pack identity coverage mismatch"
    );
    ensure!(
        pack_digests
            .values()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            == expected_packs.len(),
        "distinct packs must carry distinct fixture digests"
    );
    Ok(())
}

fn expected_pack_identity(pack_id: &str) -> anyhow::Result<PackIdentity> {
    let pack = match pack_id {
        crate::m15_pack::RETRY_DIAG => crate::long_live::m15_diag_pack(),
        crate::m15_pack::RETRY_MIGRATE => crate::long_live::m15_migrate_pack(),
        "retry_policy_dev" => crate::long_live::retry_pack(),
        _ => anyhow::bail!("unknown frozen pack {pack_id}"),
    };
    let fixture_sha256 = (pack.identity_sha256)();
    let exact_recipe_inputs = (pack.exact_recipe_inputs)();
    let surface_config_digest = serialized_digest(&serde_json::json!({
        "profile": FORMAL_TOOL_SURFACE,
        "recovery_surface": false,
        "exact_recipe_inputs": exact_recipe_inputs,
    }))?;
    let directive_sha256 = format!("{:x}", Sha256::digest((pack.directive)().as_bytes()));
    let prompt_config_digest = serialized_digest(&serde_json::json!({
        "candidate": FORMAL_CANDIDATE_ID,
        "directive_sha256": directive_sha256,
        "acceptance_declaration": pack.acceptance_declaration,
        "acceptance_domain": pack.acceptance_domain,
        "acceptance_profile": AcceptanceProfile::M15V1.id(),
        "completion_opportunity": false,
        "project_task_progress": true,
        "settlement_projection_diagnostics": false,
    }))?;
    let (_, authority) = crate::long_live::pack_verification_projection(&pack);
    let acceptance_authority = AcceptanceAuthorityIdentity {
        domain: pack.acceptance_domain.map(str::to_owned),
        declaration_revision: authority
            .as_ref()
            .map(|declaration| declaration.declaration_revision),
        source_digest: authority.map(|declaration| declaration.source_digest),
    };
    Ok(PackIdentity {
        fixture_sha256,
        surface_config_digest,
        prompt_config_digest,
        acceptance_authority,
    })
}

fn provider_config_digest(manifest: &CellManifest) -> anyhow::Result<String> {
    let model = manifest
        .openai_model
        .as_deref()
        .filter(|value| non_empty(value))
        .context("cell manifest is missing model identity")?;
    let base_url = manifest
        .openai_base_url
        .as_deref()
        .filter(|value| non_empty(value))
        .context("cell manifest is missing provider base-url identity")?;
    let protocol = manifest
        .openai_protocol
        .as_deref()
        .filter(|value| non_empty(value))
        .context("cell manifest is missing provider protocol identity")?;
    let context_window = manifest
        .openai_context_window
        .as_deref()
        .filter(|value| non_empty(value))
        .context("cell manifest is missing provider context-window identity")?;
    serialized_digest(&serde_json::json!({
        "model": model,
        "base_url": base_url,
        "protocol": protocol,
        "context_window": context_window,
    }))
}

fn validate_acceptance_authority(
    dimensions: &Dimensions,
    expected: &AcceptanceAuthorityIdentity,
) -> anyhow::Result<()> {
    let observed = AcceptanceAuthorityIdentity {
        domain: dimensions.acceptance_domain.clone(),
        declaration_revision: dimensions.acceptance_declaration_revision,
        source_digest: dimensions.acceptance_source_digest.clone(),
    };
    ensure!(
        observed == *expected,
        "acceptance authority identity drift for {}",
        dimensions.pack_id
    );
    match expected.domain.as_deref() {
        Some(_) => {
            ensure!(
                expected
                    .declaration_revision
                    .is_some_and(|revision| revision > 0),
                "frozen acceptance declaration revision is invalid for {}",
                dimensions.pack_id
            );
            ensure!(
                expected.source_digest.as_deref().is_some_and(is_sha256),
                "frozen acceptance source digest is malformed for {}",
                dimensions.pack_id
            );
        }
        None => ensure!(
            expected.declaration_revision.is_none() && expected.source_digest.is_none(),
            "frozen pack {} has a partial acceptance authority",
            dimensions.pack_id
        ),
    }
    Ok(())
}

fn serialized_digest(value: &impl Serialize) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(value).context("serialize formal identity")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn non_empty(value: &str) -> bool {
    !value.trim().is_empty()
}

fn is_sha256(value: &str) -> bool {
    value.parse::<agent_contracts::ContentDigest>().is_ok()
}

fn metric_u64(summary: &CellSummary, name: &str) -> anyhow::Result<u64> {
    summary
        .metrics
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .with_context(|| {
            format!("cell summary metric {name} is missing or is not an unsigned integer")
        })
}

fn render_cells(cells: &[Cell]) -> RenderedWindow {
    let mut ordered: Vec<&Cell> = cells.iter().collect();
    ordered.sort_by_key(|cell| {
        (
            cell.dimensions.pack_id.as_str(),
            cell.dimensions.mode.id(),
            cell.manifest.repeat,
        )
    });
    let passed = ordered
        .iter()
        .filter(|cell| cell.dimensions.verdict == CellVerdict::Pass)
        .count();
    let not_run = ordered
        .iter()
        .filter(|cell| cell.dimensions.verdict == CellVerdict::NotRun)
        .count();
    let closures = ordered
        .iter()
        .filter(|cell| cell.dimensions.task_completed)
        .count();
    let behavior_pass = ordered
        .iter()
        .filter(|cell| cell.dimensions.behavioral_oracle == "pass")
        .count();
    let metric = |cell: &&Cell, name: &str| {
        cell.summary
            .metrics
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
    };
    let total_rounds: u64 = ordered.iter().map(|cell| metric(cell, "rounds")).sum();
    let total_tools: u64 = ordered.iter().map(|cell| metric(cell, "tool_calls")).sum();
    let total_input_tokens: u64 = ordered
        .iter()
        .map(|cell| metric(cell, "model_input_tokens"))
        .sum();
    let total_output_tokens: u64 = ordered
        .iter()
        .map(|cell| metric(cell, "model_output_tokens"))
        .sum();
    let total_cached_input_tokens: u64 = ordered
        .iter()
        .map(|cell| metric(cell, "model_cached_input_tokens"))
        .sum();
    let total_schema_tokens: u64 = ordered
        .iter()
        .map(|cell| metric(cell, "schema_tokens_total"))
        .sum();
    let max_rounds = ordered
        .iter()
        .map(|cell| metric(cell, "rounds"))
        .max()
        .unwrap_or_default();
    let max_tools = ordered
        .iter()
        .map(|cell| metric(cell, "tool_calls"))
        .max()
        .unwrap_or_default();
    let max_wall_ms = ordered
        .iter()
        .map(|cell| cell.summary.wall_ms)
        .max()
        .unwrap_or_default();
    let censored = not_run > 0;
    let plane_passed = !censored && passed == ordered.len();
    let verdict = if censored {
        "CENSORED"
    } else if plane_passed {
        "PASS"
    } else {
        "FAILED"
    };
    let mut markdown = format!(
        "# M15 development window — {verdict}\n\nSchema `{WINDOW_SCHEMA}`. Generated mechanically from immutable cell bundles.\n\n| cell | behavior | diff | closure | continuation | provider | rounds | tools | wall ms | verdict |\n| --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | --- |\n"
    );
    for cell in &ordered {
        let d = &cell.dimensions;
        markdown.push_str(&format!(
            "| {} {} r{} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            d.pack_id,
            d.mode.id(),
            cell.manifest.repeat,
            d.behavioral_oracle,
            d.allowed_diff,
            d.task_closure,
            d.continuation,
            d.provider_runtime,
            metric(cell, "rounds"),
            metric(cell, "tool_calls"),
            cell.summary.wall_ms,
            d.verdict.id(),
        ));
    }
    markdown.push_str(&format!(
        "\nSummary: pass {passed}/{total}; NOT_RUN {not_run}/{total}; behavior pass {behavior_pass}/{total}; closures {closures}/{total}.\n\nEfficiency facts: rounds total/max {total_rounds}/{max_rounds}; tool calls total/max {total_tools}/{max_tools}; wall max {max_wall_ms} ms; provider input/output tokens {total_input_tokens}/{total_output_tokens} (cached input {total_cached_input_tokens}); schema tokens {total_schema_tokens}. Token totals remain lower bounds when a cell's summary marks provider usage incomplete.\n",
        total = ordered.len()
    ));
    RenderedWindow {
        markdown,
        passed: plane_passed,
        censored,
    }
}

fn read_dimensions(path: &Path) -> anyhow::Result<Dimensions> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))?;
    let object = value
        .as_object()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let missing: Vec<_> = REQUIRED_DIMENSION_IDENTITY_FIELDS
        .iter()
        .copied()
        .filter(|field| !object.contains_key(*field))
        .collect();
    ensure!(
        missing.is_empty(),
        "{} is missing required v4 identity fields: {}",
        path.display(),
        missing.join(", ")
    );
    serde_json::from_value(value).with_context(|| format!("decode {}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("decode {}", path.display()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_cell(
        root: &Path,
        pack: &str,
        mode: PilotMode,
        repeat: u32,
        failure: Option<CellFailureClass>,
    ) -> PathBuf {
        let dir = root
            .join(format!("m15-{pack}-{}", mode.id()))
            .join(format!("r{repeat}"))
            .join("dynamic");
        std::fs::create_dir_all(&dir).unwrap();
        let not_run = failure == Some(CellFailureClass::ProviderTransport);
        let behavior = if not_run { "not_run" } else { "pass" };
        let exact = mode == PilotMode::Resume;
        let verdict = evaluate_verdict(
            AcceptanceProfile::M15V1,
            mode,
            failure,
            behavior,
            true,
            exact,
            exact,
            exact,
            false,
        );
        let expected_identity = expected_pack_identity(pack).unwrap();
        let manifest = CellManifest {
            schema: crate::bundle::CELL_SCHEMA.into(),
            fixture_id: pack.into(),
            engine: "dynamic".into(),
            repeat,
            repeats: 2,
            live: true,
            tool_surface: Some("production".into()),
            fixture_sha256: expected_identity.fixture_sha256.clone(),
            git_head: Some("head".into()),
            git_dirty: Some(false),
            git_dirty_sha256: Some("clean".into()),
            source_tree_digest: Some("1".repeat(64)),
            openai_model: Some("model".into()),
            openai_base_url: Some("provider".into()),
            openai_protocol: Some("responses".into()),
            openai_context_window: Some("128000".into()),
        };
        let dimensions = Dimensions {
            schema: PILOT_SCHEMA.into(),
            pack_id: pack.into(),
            candidate_id: FORMAL_CANDIDATE_ID.into(),
            source_tree_digest: manifest.source_tree_digest.clone(),
            fixture_sha256: manifest.fixture_sha256.clone(),
            repeat,
            tool_surface: FORMAL_TOOL_SURFACE.into(),
            surface_config_digest: expected_identity.surface_config_digest,
            prompt_config_digest: expected_identity.prompt_config_digest,
            acceptance_domain: expected_identity.acceptance_authority.domain,
            acceptance_declaration_revision: expected_identity
                .acceptance_authority
                .declaration_revision,
            acceptance_source_digest: expected_identity.acceptance_authority.source_digest,
            provider_config_digest: provider_config_digest(&manifest).unwrap(),
            settlement_projection_diagnostics: false,
            mode,
            acceptance_profile: AcceptanceProfile::M15V1,
            verdict,
            behavioral_oracle: behavior.into(),
            observed_behavioral_oracle: if not_run { "fail" } else { "pass" }.into(),
            allowed_diff: "pass".into(),
            task_closure: if failure.is_some() {
                "failed"
            } else {
                "active"
            }
            .into(),
            continuation: if exact {
                "restored_and_continued"
            } else {
                "n/a"
            }
            .into(),
            exact_resume_tuple_matched: exact,
            restored: exact,
            continued: exact,
            turn_completed: failure.is_none(),
            task_completed: false,
            wall_ms: 10,
            model_rounds_phase_one: 1,
            model_rounds_phase_two: 0,
            resume_committed: u64::from(exact),
            checkpoint_durable: u64::from(exact),
            provider_runtime: if not_run {
                "transport_failed"
            } else {
                "healthy"
            }
            .into(),
            final_passed: verdict == CellVerdict::Pass,
            runtime_error: failure.map(|_| "provider failed".into()),
            runtime_error_class: failure,
            runtime_error_retryable: failure == Some(CellFailureClass::ProviderTransport),
            completion_opportunity: "off".into(),
            recovery_surface: "off".into(),
            project_task_progress: true,
            project_settlement: false,
        };
        let summary = CellSummary {
            schema: CELL_SCHEMA.into(),
            outcome: if verdict == CellVerdict::Pass {
                "passed"
            } else {
                "error"
            }
            .into(),
            error: dimensions.runtime_error.clone(),
            passed: verdict == CellVerdict::Pass,
            wall_ms: 10,
            seq_contiguous: true,
            seq_gap: None,
            broadcast_lagged: 0,
            model_deltas_omitted: 0,
            model_started: 1,
            model_used: 1,
            usage_incomplete: false,
            provider_tokens_lower_bound: false,
            workspace_sha256: "0".repeat(64),
            workspace_files: 1,
            tools: Vec::new(),
            metrics: serde_json::json!({
                "rounds": 1,
                "tool_calls": 1,
                "model_input_tokens": 10,
                "model_output_tokens": 5,
                "schema_tokens_total": 2
            }),
        };
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("dimensions.json"),
            serde_json::to_vec_pretty(&dimensions).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("summary.json"),
            serde_json::to_vec_pretty(&summary).unwrap(),
        )
        .unwrap();
        dir
    }

    fn full_window(root: &Path, failed_cell: bool) -> Vec<PathBuf> {
        let mut cells = Vec::new();
        for pack in ["retry_diag_dev", "retry_migrate_dev", "retry_policy_dev"] {
            for repeat in 1..=2 {
                for mode in [PilotMode::Normal, PilotMode::Resume] {
                    let failure = (failed_cell
                        && pack == "retry_policy_dev"
                        && repeat == 2
                        && mode == PilotMode::Resume)
                        .then_some(CellFailureClass::ProviderTransport);
                    cells.push(write_cell(root, pack, mode, repeat, failure));
                }
            }
        }
        cells
    }

    #[test]
    fn report_recomputes_a_closure_free_m15_pass() {
        let temp = tempfile::tempdir().unwrap();
        let cells = full_window(temp.path(), false);
        let (_, rendered) = persist_window(
            temp.path(),
            &cells,
            &["retry_diag_dev", "retry_migrate_dev", "retry_policy_dev"],
            2,
        )
        .unwrap();
        assert!(rendered.passed);
        assert!(!rendered.censored);
        assert!(rendered.markdown.contains("pass 12/12"));
        assert!(rendered.markdown.contains("closures 0/12"));
    }

    #[test]
    fn provider_not_run_censors_the_mechanical_window() {
        let temp = tempfile::tempdir().unwrap();
        let cells = full_window(temp.path(), true);
        let (_, rendered) = persist_window(
            temp.path(),
            &cells,
            &["retry_diag_dev", "retry_migrate_dev", "retry_policy_dev"],
            2,
        )
        .unwrap();
        assert!(!rendered.passed);
        assert!(rendered.censored);
        assert!(rendered.markdown.contains("NOT_RUN 1/12"));
    }

    #[test]
    fn formal_report_rejects_a_partial_pack() {
        let temp = tempfile::tempdir().unwrap();
        let cells: Vec<PathBuf> = full_window(temp.path(), false)
            .into_iter()
            .filter(|path| path.to_string_lossy().contains("retry_diag_dev"))
            .collect();
        let error = persist_window(temp.path(), &cells, &["retry_diag_dev"], 2)
            .err()
            .expect("partial pack must be rejected");
        assert!(error.to_string().contains("complete frozen three-pack"));
    }

    #[test]
    fn report_rejects_event_summary_drift() {
        let temp = tempfile::tempdir().unwrap();
        let cells = full_window(temp.path(), false);
        let summary_path = cells[0].join("summary.json");
        let mut summary: CellSummary = read_json(&summary_path).unwrap();
        summary.seq_contiguous = false;
        std::fs::write(&summary_path, serde_json::to_vec_pretty(&summary).unwrap()).unwrap();
        let error = persist_window(
            temp.path(),
            &cells,
            &M15_PACK_IDS,
            crate::long_live::DEFAULT_REPEATS,
        )
        .err()
        .expect("summary drift must be rejected");
        assert!(error.to_string().contains("not contiguous"));
    }

    #[test]
    fn report_rejects_acceptance_authority_drift_within_a_pack() {
        let temp = tempfile::tempdir().unwrap();
        let cells = full_window(temp.path(), false);
        let cell = cells
            .iter()
            .find(|path| path.to_string_lossy().contains("retry_policy_dev"))
            .unwrap();
        let dimensions_path = cell.join("dimensions.json");
        let mut dimensions: serde_json::Value = read_json(&dimensions_path).unwrap();
        dimensions["acceptance_source_digest"] = serde_json::json!("b".repeat(64));
        std::fs::write(
            &dimensions_path,
            serde_json::to_vec_pretty(&dimensions).unwrap(),
        )
        .unwrap();

        let error = persist_window(
            temp.path(),
            &cells,
            &M15_PACK_IDS,
            crate::long_live::DEFAULT_REPEATS,
        )
        .err()
        .expect("acceptance authority drift must be rejected");
        assert!(
            error
                .to_string()
                .contains("acceptance authority identity drift")
        );
    }

    #[test]
    fn report_rejects_formal_switch_drift() {
        let temp = tempfile::tempdir().unwrap();
        let cells = full_window(temp.path(), false);
        let dimensions_path = cells[0].join("dimensions.json");
        let mut dimensions: serde_json::Value = read_json(&dimensions_path).unwrap();
        dimensions["project_settlement"] = serde_json::json!(true);
        std::fs::write(
            &dimensions_path,
            serde_json::to_vec_pretty(&dimensions).unwrap(),
        )
        .unwrap();

        let error = persist_window(
            temp.path(),
            &cells,
            &M15_PACK_IDS,
            crate::long_live::DEFAULT_REPEATS,
        )
        .err()
        .expect("switch drift must be rejected");
        assert!(error.to_string().contains("settlement projection off"));
    }

    #[test]
    fn report_rejects_prompt_identity_drift() {
        let temp = tempfile::tempdir().unwrap();
        let cells = full_window(temp.path(), false);
        let dimensions_path = cells[0].join("dimensions.json");
        let mut dimensions: serde_json::Value = read_json(&dimensions_path).unwrap();
        dimensions["prompt_config_digest"] = serde_json::json!("f".repeat(64));
        std::fs::write(
            &dimensions_path,
            serde_json::to_vec_pretty(&dimensions).unwrap(),
        )
        .unwrap();

        let error = persist_window(
            temp.path(),
            &cells,
            &M15_PACK_IDS,
            crate::long_live::DEFAULT_REPEATS,
        )
        .err()
        .expect("prompt identity drift must be rejected");
        assert!(
            error
                .to_string()
                .contains("prompt configuration identity drift")
        );
    }

    #[test]
    fn report_rejects_whitespace_padded_auto_protocol() {
        let temp = tempfile::tempdir().unwrap();
        let cells = full_window(temp.path(), false);
        let manifest_path = cells[0].join("manifest.json");
        let dimensions_path = cells[0].join("dimensions.json");
        let mut manifest: CellManifest = read_json(&manifest_path).unwrap();
        manifest.openai_protocol = Some(" auto ".into());
        let mut dimensions: Dimensions = read_json(&dimensions_path).unwrap();
        dimensions.provider_config_digest = provider_config_digest(&manifest).unwrap();
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &dimensions_path,
            serde_json::to_vec_pretty(&dimensions).unwrap(),
        )
        .unwrap();

        let error = persist_window(
            temp.path(),
            &cells,
            &M15_PACK_IDS,
            crate::long_live::DEFAULT_REPEATS,
        )
        .err()
        .expect("auto protocol must be rejected after trimming");
        assert!(error.to_string().contains("auto protocol negotiation"));
    }

    #[test]
    fn report_rejects_a_missing_nullable_identity_field() {
        let temp = tempfile::tempdir().unwrap();
        let cells = full_window(temp.path(), false);
        let dimensions_path = cells[0].join("dimensions.json");
        let mut dimensions: serde_json::Value = read_json(&dimensions_path).unwrap();
        dimensions
            .as_object_mut()
            .unwrap()
            .remove("acceptance_domain");
        std::fs::write(
            &dimensions_path,
            serde_json::to_vec_pretty(&dimensions).unwrap(),
        )
        .unwrap();

        let error = persist_window(
            temp.path(),
            &cells,
            &M15_PACK_IDS,
            crate::long_live::DEFAULT_REPEATS,
        )
        .err()
        .expect("missing v4 identity must be rejected");
        assert!(error.to_string().contains("missing required v4 identity"));
    }
}
