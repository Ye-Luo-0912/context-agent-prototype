//! C-hygiene engine-only ablation (P3 descriptor-only recall, P4 file-body
//! cap). No provider, no frozen Context Bench SPEC.
//!
//! Arms share one scripted trajectory so the delta is the config, not the
//! model. `current` is production C. The other two arms stay off by default
//! in `SimpleContextConfig`.

use agent_contracts::{
    ContextEngine, ContextIngress, ContextKind, ContextMaintenanceTrigger, ContextQuery,
    FocusState, TaskId, ToolOutput,
};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::json;

/// Production C plus the two experimental C-hygiene switches.
pub const HYGIENE_ARMS: [&str; 3] = ["current", "descriptor-only", "one-file-body"];

const SECRET_BODY: &str = "hygiene-secret-body-v1";

/// One arm's measurements from the two scripted scenarios.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HygieneArmReport {
    pub arm: &'static str,
    /// Full-GC evictions in the reactivation scenario (must be ≥1).
    pub evicted: u64,
    /// ToolObservation bodies pulled back because entities were hot again.
    pub hot_reactivated_tool_observations: u64,
    /// Whether the old tool stdout token was selected after that GC pass.
    pub secret_body_selected: bool,
    /// Last materialize snapshot: selected tokens marked reactivated.
    pub selected_tokens_reactivated: u64,
    /// File-body items still in the post-GC snapshot (P4).
    pub file_bodies_selected: u64,
    pub reread_previously_selected: u64,
    pub reread_resident_unselected: u64,
    pub reread_warm: u64,
    pub reread_stored: u64,
    pub reread_first_read: u64,
}

impl HygieneArmReport {
    pub fn gc_caused_rereads(&self) -> u64 {
        self.reread_warm.saturating_add(self.reread_stored)
    }
}

/// Config for one C-hygiene arm. Unknown names fall back to production C.
pub fn hygiene_config(arm: &str) -> SimpleContextConfig {
    match arm {
        "descriptor-only" => SimpleContextConfig {
            descriptor_only_tool_observation_reactivation: true,
            ..SimpleContextConfig::default()
        },
        "one-file-body" => SimpleContextConfig {
            recent_file_bodies: 1,
            recent_file_body_lease_turns: 1,
            ..SimpleContextConfig::default()
        },
        _ => SimpleContextConfig::default(),
    }
}

pub fn render_hygiene(reports: &[HygieneArmReport]) -> String {
    let mut out = String::from(
        "C-hygiene ablation (engine-only, no provider; SPEC untouched):\n\
         arm                secret_in_prompt  tool_obs_hot_reactivate  reactivated_tokens  file_bodies  reread(prev/resident/warm/stored/first)  gc_reread\n",
    );
    for report in reports {
        out.push_str(&format!(
            "{:<18} {:<17} {:<24} {:<19} {:<12} {}/{}/{}/{}/{}{:>18}\n",
            report.arm,
            report.secret_body_selected,
            report.hot_reactivated_tool_observations,
            report.selected_tokens_reactivated,
            report.file_bodies_selected,
            report.reread_previously_selected,
            report.reread_resident_unselected,
            report.reread_warm,
            report.reread_stored,
            report.reread_first_read,
            report.gc_caused_rereads(),
        ));
    }
    out.push_str(
        "Read: secret_in_prompt = old fs.read body auto-returned (P3).\n\
         gc_reread = warm+stored fs.read (P4 / GC-caused extra reads).\n",
    );
    out
}

pub async fn run_hygiene_ablation() -> anyhow::Result<Vec<HygieneArmReport>> {
    let mut reports = Vec::with_capacity(HYGIENE_ARMS.len());
    for &arm in &HYGIENE_ARMS {
        reports.push(run_hygiene_arm(arm).await?);
    }
    Ok(reports)
}

async fn run_hygiene_arm(arm: &'static str) -> anyhow::Result<HygieneArmReport> {
    let reactivation = run_reactivation_scenario(hygiene_config(arm)).await?;
    let reread = run_reread_scenario(hygiene_config(arm)).await?;
    Ok(HygieneArmReport {
        arm,
        evicted: reactivation.evicted,
        hot_reactivated_tool_observations: reactivation.hot_reactivated_tool_observations,
        secret_body_selected: reactivation.secret_body_selected,
        selected_tokens_reactivated: reactivation.selected_tokens_reactivated,
        file_bodies_selected: reread.file_bodies_selected,
        reread_previously_selected: reread.reread_previously_selected,
        reread_resident_unselected: reread.reread_resident_unselected,
        reread_warm: reread.reread_warm,
        reread_stored: reread.reread_stored,
        reread_first_read: reread.reread_first_read,
    })
}

struct ReactivationStats {
    evicted: u64,
    hot_reactivated_tool_observations: u64,
    secret_body_selected: bool,
    selected_tokens_reactivated: u64,
}

struct RereadStats {
    file_bodies_selected: u64,
    reread_previously_selected: u64,
    reread_resident_unselected: u64,
    reread_warm: u64,
    reread_stored: u64,
    reread_first_read: u64,
}

/// Old fs.read body is evicted, then the user names the same file again.
/// Production C auto-reactivates file bodies; descriptor-only must not.
/// Stamped-path shell logs are identity and never hot-recall.
async fn run_reactivation_scenario(
    config: SimpleContextConfig,
) -> anyhow::Result<ReactivationStats> {
    let engine = SimpleContextEngine::new(config);
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await?;
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: "hygiene-obs-1".into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: "read".into(),
                model_content: format!("     1 | {SECRET_BODY}"),
                artifact_ref: None,
                metadata: json!({"path": "AuthService.rs", "revision": "hygiene-rev"}),
            },
            scope_id: None,
        })
        .await?;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await?;
    let evicted = engine.gc().await?;
    anyhow::ensure!(
        evicted.evicted >= 1,
        "reactivation scenario must evict first: {evicted:?}"
    );

    engine
        .ingest(ContextIngress::UserMessage {
            content: "what did we change in AuthService.rs?".into(),
        })
        .await?;
    let recalled = engine.gc().await?;
    let hot_reactivated_tool_observations = recalled
        .reactivations
        .iter()
        .filter(|row| row.kind == ContextKind::ToolObservation && row.reason.contains("hot again"))
        .count() as u64;

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "what did we change in AuthService.rs?".into(),
            budget_tokens: 8_000,
            hints: Default::default(),
        })
        .await?;
    let secret_body_selected = snapshot
        .items
        .iter()
        .any(|item| item.content.contains(SECRET_BODY));
    let selected_tokens_reactivated = snapshot.diagnostics.reactivation_selected_tokens;

    Ok(ReactivationStats {
        evicted: evicted.evicted as u64,
        hot_reactivated_tool_observations,
        secret_body_selected,
        selected_tokens_reactivated,
    })
}

/// Three file bodies, then a reread of the first path after tool-hot TTL.
/// Cap=8 keeps the old body as a latest-file root; cap=1 + one-round lease
/// lets GC take it, so the reread is warm/stored instead of previously-selected.
async fn run_reread_scenario(config: SimpleContextConfig) -> anyhow::Result<RereadStats> {
    let engine = SimpleContextEngine::new(config);
    engine
        .ingest(ContextIngress::FocusChanged {
            focus: FocusState::for_task(TaskId::new(), "inspect three modules"),
        })
        .await?;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "inspect the three modules".into(),
        })
        .await?;
    ingest_fs_read(&engine, "1", "src/a.rs", "fn alpha() {}").await?;
    ingest_fs_read(&engine, "2", "src/b.rs", "fn bravo() {}").await?;
    ingest_fs_read(&engine, "3", "src/c.rs", "fn charlie() {}").await?;
    engine
        .materialize(ContextQuery {
            current_input: "inspect the three modules".into(),
            budget_tokens: 8_000,
            hints: Default::default(),
        })
        .await?;
    // Expire tool-hot (default TTL 2 user turns) so GC cannot immediately
    // resurrect evicted file bodies via the paths those reads just stamped.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "plain continuation".into(),
        })
        .await?;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "still plain continuation".into(),
        })
        .await?;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await?;
    engine.gc().await?;

    let snapshot = engine
        .materialize(ContextQuery {
            current_input: "still plain continuation".into(),
            budget_tokens: 8_000,
            hints: Default::default(),
        })
        .await?;
    let file_bodies_selected = snapshot
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.file_path.as_deref(),
                Some("src/a.rs" | "src/b.rs" | "src/c.rs")
            )
        })
        .count() as u64;

    ingest_fs_read(&engine, "4", "src/a.rs", "fn alpha() {}").await?;
    let diagnostics = engine.diagnostics().await?;
    Ok(RereadStats {
        file_bodies_selected,
        reread_previously_selected: diagnostics.reread_previously_selected,
        reread_resident_unselected: diagnostics.reread_resident_unselected,
        reread_warm: diagnostics.reread_warm,
        reread_stored: diagnostics.reread_stored,
        reread_first_read: diagnostics.reread_first_read,
    })
}

async fn ingest_fs_read(
    engine: &SimpleContextEngine,
    call_id: &str,
    path: &str,
    body: &str,
) -> anyhow::Result<()> {
    engine
        .ingest(ContextIngress::ToolObservation {
            output: ToolOutput {
                call_id: call_id.into(),
                tool_name: "fs.read".into(),
                ok: true,
                summary: format!("read of {path}"),
                model_content: format!("     1 | {body}"),
                artifact_ref: None,
                metadata: json!({ "path": path, "revision": "hygiene-rev" }),
            },
            scope_id: None,
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hygiene_ablation_descriptor_only_drops_old_tool_bodies() {
        let reports = run_hygiene_ablation().await.expect("hygiene");
        let current = arm(&reports, "current");
        let descriptor = arm(&reports, "descriptor-only");
        assert!(
            current.evicted >= 1 && descriptor.evicted >= 1,
            "both arms must evict before recall: {reports:?}"
        );
        assert!(
            current.secret_body_selected,
            "production C still auto-returns old file-body ToolObservation: {current:?}"
        );
        assert!(
            current.hot_reactivated_tool_observations >= 1,
            "production C hot-reactivates ToolObservation: {current:?}"
        );
        assert!(
            !descriptor.secret_body_selected,
            "P3 must keep the old tool body out of the prompt: {descriptor:?}"
        );
        assert_eq!(
            descriptor.hot_reactivated_tool_observations, 0,
            "P3 must not hot-reactivate ToolObservation bodies: {descriptor:?}"
        );
    }

    #[tokio::test]
    async fn hygiene_ablation_one_file_body_moves_reread_onto_gc() {
        let reports = run_hygiene_ablation().await.expect("hygiene");
        let current = arm(&reports, "current");
        let one_body = arm(&reports, "one-file-body");
        assert!(
            current.file_bodies_selected >= 2,
            "cap=8 must keep recent file bodies after GC: {current:?}"
        );
        assert_eq!(
            current.reread_previously_selected, 1,
            "cap=8 reread of a selected body is previously-selected: {current:?}"
        );
        assert_eq!(
            current.gc_caused_rereads(),
            0,
            "cap=8 must not reread from warm/stored: {current:?}"
        );
        assert!(
            one_body.file_bodies_selected < current.file_bodies_selected,
            "cap=1 + one-round lease must drop long-lived file-body roots: current={current:?} one={one_body:?}"
        );
        assert_eq!(
            one_body.reread_previously_selected, 0,
            "dropped file-body roots must not count as previously-selected: {one_body:?}"
        );
        assert!(
            one_body.gc_caused_rereads() >= 1,
            "P4 should turn the extra read into warm/stored: {one_body:?}"
        );
    }

    #[test]
    fn hygiene_render_names_the_arms() {
        let rendered = render_hygiene(&[HygieneArmReport {
            arm: "current",
            evicted: 1,
            hot_reactivated_tool_observations: 1,
            secret_body_selected: true,
            selected_tokens_reactivated: 10,
            file_bodies_selected: 3,
            reread_previously_selected: 1,
            reread_resident_unselected: 0,
            reread_warm: 0,
            reread_stored: 0,
            reread_first_read: 3,
        }]);
        assert!(rendered.contains("current"));
        assert!(rendered.contains("secret_in_prompt"));
        assert!(rendered.contains("gc_reread"));
    }

    fn arm<'a>(reports: &'a [HygieneArmReport], name: &str) -> &'a HygieneArmReport {
        reports
            .iter()
            .find(|report| report.arm == name)
            .unwrap_or_else(|| panic!("missing arm {name}: {reports:?}"))
    }
}
