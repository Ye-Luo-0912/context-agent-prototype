//! Mechanism V2: three one-mechanism scenarios. Independent of frozen
//! `agent-eval.context-bench.v1`. Structural CI is pack self-check plus
//! unit tests; live `--context-mech-run` is the next mechanism instrument
//! (`late_semantic_constraint` is the non-Anchor GC-recall test): A/C ×
//! 3 tasks × 2 repeats = 12 cells. Do not keep live-running
//! `recall_after_fix`; that fixture's diagnostic mission is complete.
//! Keep its scripted `--compare-arm` tests.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::context_bench::{self, BenchPack, BenchTask, BenchTaskFile};

pub const SCHEMA: &str = "agent-eval.context-mech.v2";
pub const MECH_TASKS: usize = 3;
pub const MECH_ENGINES: usize = 2;
pub const DEFAULT_REPEATS: u32 = 2;
pub const LIVE_CELLS: usize = MECH_TASKS * MECH_ENGINES * DEFAULT_REPEATS as usize;
pub const TASK_IDS: [&str; 3] = [
    "late_semantic_constraint",
    "resume_operational_state",
    "no_semantic_episode",
];

pub const SPEC: &str = "\
schema=agent-eval.context-mech.v2
question=does each Context V1 mechanism hold in isolation
primary=per-mechanism structural test; live A/C two repeats for development judgment
tasks=late_semantic_constraint, resume_operational_state, no_semantic_episode
engines=append vs dynamic
repeats=2
cells=12
frozen_context_bench_v1=untouched
semantic_recall_v1=long-protocol trajectory only; do not keep live-running it
recall_after_fix=diagnostic complete; keep scripted tests; do not keep live-running it
next_live=context-mech.v2; late_semantic_constraint is the non-Anchor recall test
";

pub fn mech_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("context-mech")
}

#[derive(Debug, Deserialize)]
struct PackFile {
    schema: String,
    tasks: Vec<String>,
}

pub fn load_pack() -> anyhow::Result<BenchPack> {
    let root = mech_root();
    let pack_path = root.join("pack.json");
    let pack: PackFile = serde_json::from_str(
        &fs::read_to_string(&pack_path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", pack_path.display()))?,
    )?;
    if pack.schema != SCHEMA {
        anyhow::bail!("context-mech pack schema {} != {SCHEMA}", pack.schema);
    }
    let mut tasks = Vec::new();
    for rel in &pack.tasks {
        let path = root.join(rel);
        let file: BenchTaskFile = serde_json::from_str(
            &fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?,
        )?;
        tasks.push(BenchTask { file, path });
    }
    if tasks.len() != MECH_TASKS {
        anyhow::bail!(
            "context-mech pack has {} tasks, expected {MECH_TASKS}",
            tasks.len()
        );
    }
    for id in TASK_IDS {
        if tasks.iter().all(|task| task.id() != id) {
            anyhow::bail!("context-mech pack is missing {id}");
        }
    }
    Ok(BenchPack {
        root: root.to_path_buf(),
        tasks,
    })
}

pub fn spec_sha256() -> String {
    hex_encode(Sha256::digest(SPEC.as_bytes()))
}

pub fn render_pack(pack: &BenchPack) -> String {
    let mut out = String::new();
    out.push_str(&format!("schema={SCHEMA}\n"));
    out.push_str(&format!("spec_sha256={}\n", spec_sha256()));
    out.push_str("decision_instrument=context-mech.v2 (not frozen context-bench.v1)\n");
    out.push_str("engines=append vs dynamic\n");
    out.push_str(&format!("repeats={DEFAULT_REPEATS}\n"));
    out.push_str(&format!("cells={LIVE_CELLS}\n"));
    out.push_str("spec:\n");
    for line in SPEC.lines() {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("tasks:\n");
    for task in &pack.tasks {
        out.push_str(&format!(
            "  {:<28} mechanism={:<20} ops={}  {}\n",
            task.id(),
            task.file.scenario,
            task.file.ops.len(),
            task.file.name
        ));
    }
    out
}

pub fn check_pack(pack: &BenchPack) -> anyhow::Result<String> {
    let mut out = String::new();
    for task in &pack.tasks {
        let dir = tempfile::tempdir()?;
        context_bench::seed_task(pack, task, dir.path())?;
        let seeded = context_bench::evaluate_task(pack, task, dir.path());
        if seeded.assertions.iter().all(|row| row.passed) && !task.file.hidden.is_empty() {
            anyhow::bail!(
                "{} passes file asserts on the seed — the hidden check does not test the change",
                task.id()
            );
        }
        context_bench::apply_golden(pack, task, dir.path())?;
        let gold = context_bench::evaluate_task(pack, task, dir.path());
        if !gold.assertions.iter().all(|row| row.passed) {
            let misses: Vec<_> = gold
                .assertions
                .iter()
                .filter(|row| !row.passed)
                .map(|row| format!("{} {}", row.path, row.pred))
                .collect();
            anyhow::bail!(
                "{} golden failed file asserts: {}",
                task.id(),
                misses.join(", ")
            );
        }
        if let Some(failed) = gold.commands.iter().find(|row| !row.passed) {
            anyhow::bail!(
                "{} golden hidden command failed exit={:?} stderr={}",
                task.id(),
                failed.exit,
                failed.stderr
            );
        }
        if !seeded.commands.is_empty() && seeded.commands.iter().all(|row| row.passed) {
            anyhow::bail!(
                "{} hidden commands pass on the seed — the command does not test the change",
                task.id()
            );
        }
        out.push_str(&format!("ok {}\n", task.id()));
    }
    Ok(out)
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn pack_self_check() {
        let pack = load_pack().expect("context-mech pack");
        let report = check_pack(&pack).expect("mech pack self-check");
        assert!(report.contains("ok late_semantic_constraint"));
        assert!(report.contains("ok resume_operational_state"));
        assert!(report.contains("ok no_semantic_episode"));
        assert_eq!(report.lines().count(), MECH_TASKS);
        assert_eq!(LIVE_CELLS, 12);
        let rendered = render_pack(&pack);
        assert!(rendered.contains("engines=append vs dynamic"));
        assert!(rendered.contains("cells=12"));
        for task in &pack.tasks {
            assert!(!task.include_rolling(), "{} is A/C, not rolling", task.id());
        }
    }

    #[test]
    fn verbal_constraints_are_not_planted_in_seeds() {
        let pack = load_pack().expect("context-mech pack");
        let forbidden = ["unversioned ping", "old clients send"];
        for task in &pack.tasks {
            let seed = pack.root.join("seeds").join(&task.file.seed);
            let blob = read_tree(&seed);
            for needle in forbidden {
                assert!(
                    !blob
                        .to_ascii_lowercase()
                        .contains(&needle.to_ascii_lowercase()),
                    "{} seed contains verbal constraint {:?}",
                    task.id(),
                    needle
                );
            }
        }
    }

    fn read_tree(root: &Path) -> String {
        let mut out = String::new();
        for entry in walkdir(root) {
            if entry.is_file()
                && let Ok(text) = fs::read_to_string(&entry)
            {
                out.push_str(&text);
                out.push('\n');
            }
        }
        out
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        fn rec(dir: &Path, files: &mut Vec<PathBuf>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    rec(&path, files);
                } else {
                    files.push(path);
                }
            }
        }
        rec(root, &mut files);
        files.sort();
        files
    }
}
