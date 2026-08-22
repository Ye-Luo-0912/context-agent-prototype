//! Long-flow live diagnostic: one 15-turn late-constraint trajectory on
//! A/C engines executed concurrently
//! ([`crate::fixture_driver::compare_mech_live_parallel`]) so wall time
//! stays close to a single cell. Independent of the frozen packs; evidence
//! lands under its own evidence directory.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

pub const SCHEMA: &str = "agent-eval.longflow.v1";
const SPEC: &str = "\
schema=agent-eval.longflow.v1
question=does the late-constraint mechanism hold across a 15-turn drift
primary=development diagnostic; not an acceptance gate and never a gate input
engines=append vs dynamic, run concurrently
tasks=late_constraint_long
";

fn pack_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("longflow")
}

#[derive(Deserialize)]
struct PackFile {
    schema: String,
    tasks: Vec<String>,
}

pub fn load_pack() -> anyhow::Result<crate::context_bench::BenchPack> {
    let root = pack_root();
    let pack_path = root.join("pack.json");
    let file: PackFile = serde_json::from_str(
        &fs::read_to_string(&pack_path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", pack_path.display()))?,
    )?;
    if file.schema != SCHEMA {
        anyhow::bail!("longflow pack schema {} != {SCHEMA}", file.schema);
    }
    let mut tasks = Vec::new();
    for rel in &file.tasks {
        let path = root.join(rel);
        let task_file: crate::context_bench::BenchTaskFile = serde_json::from_str(
            &fs::read_to_string(&path)
                .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?,
        )?;
        tasks.push(crate::context_bench::BenchTask {
            file: task_file,
            path,
        });
    }
    if tasks.is_empty() {
        anyhow::bail!("longflow pack has no tasks");
    }
    Ok(crate::context_bench::BenchPack { root, tasks })
}

pub fn spec_sha256() -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(SPEC.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longflow_pack_loads_with_seed_and_check_assets() {
        let pack = load_pack().expect("the committed longflow pack must load");
        assert_eq!(pack.tasks.len(), 1);
        let task = &pack.tasks[0];
        assert_eq!(task.id(), "late_constraint_long");
        assert!(
            task.file.ops.len() >= 12,
            "the long-flow value is its drift length"
        );
        assert!(
            pack.root.join("seeds").join(&task.file.seed).is_dir(),
            "seed dir must exist for seeding"
        );
        for command in &task.file.hidden_commands {
            assert!(
                pack.root.join("checks").join(&command.script).is_file(),
                "check script {} must exist",
                command.script
            );
        }
        assert!(spec_sha256().len() == 64, "sha256 hex length");
    }
}
