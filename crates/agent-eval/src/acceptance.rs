//! 正式门禁的精确 300 题 ID 锁。
//!
//! 冻结包是 509 题；门禁不得是「任意 ≥300」。本模块在看到接受细胞之前
//! 锁死 exact 300 ids：30 道校准题全部纳入，再按
//! `sha256(agent-eval.acceptance.v1 || POWER_SEED || id)` 补到 300。
//! SWE-bench Verified 只有 45 道 large，harvest size 凑不齐 100/100/100；
//! 功效模型的三分层仍按题序，不按 harvest size。改集合必须重冻 SPEC。

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use crate::harvest;
use crate::pilot;
use crate::suite::{SuitePack, SuiteTask};

pub const ACCEPTANCE_SCHEMA: &str = "agent-eval.acceptance.v1";
pub const ACCEPTANCE_SALT: &str = "agent-eval.acceptance.v1";
const ACCEPTANCE_SEED_ASCII: &[u8] = b"20260814";
pub const ACCEPTANCE_N: usize = 300;
pub const ACCEPTANCE_PER_SIZE: usize = 100;
pub const FROZEN_ACCEPTANCE_SHA256: &str =
    "7ff6b5ddefc7e6e6dc138e5e582de75b0cfc4f5eba831385cc550e4df8c124a7";

const FROZEN_ACCEPTANCE_TXT: &str = include_str!("../suite/acceptance-ids.txt");

pub fn frozen_ids() -> Vec<&'static str> {
    FROZEN_ACCEPTANCE_TXT
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

pub fn ids_sha256(ids: &[String]) -> String {
    let mut hasher = Sha256::new();
    for id in ids {
        hasher.update(id.as_bytes());
        hasher.update(b"\n");
    }
    hex_encode(hasher.finalize())
}

fn rank_key(id: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ACCEPTANCE_SALT.as_bytes());
    hasher.update([0]);
    hasher.update(ACCEPTANCE_SEED_ASCII);
    hasher.update([0]);
    hasher.update(id.as_bytes());
    hasher.finalize().into()
}

/// 选出正式 300 题。与冻结文件不一致则失败。
pub fn select_acceptance(pack: &SuitePack) -> anyhow::Result<Vec<String>> {
    let ids = select_acceptance_ids(pack);
    if ids.len() != ACCEPTANCE_N {
        anyhow::bail!("acceptance sample n={} (want {ACCEPTANCE_N})", ids.len());
    }
    let frozen: Vec<String> = frozen_ids().into_iter().map(str::to_string).collect();
    if ids != frozen {
        anyhow::bail!(
            "acceptance ids drifted from the freeze lock; re-register before seeing cells"
        );
    }
    if ids_sha256(&ids) != FROZEN_ACCEPTANCE_SHA256 {
        anyhow::bail!("acceptance sha256 drifted");
    }
    Ok(ids)
}

pub fn select_acceptance_ids(pack: &SuitePack) -> Vec<String> {
    let mut by_id: BTreeMap<&str, &SuiteTask> = BTreeMap::new();
    for task in &pack.tasks {
        by_id.insert(task.id.as_str(), task);
    }
    let mut need: BTreeMap<&str, usize> = ["small", "medium", "large"]
        .into_iter()
        .map(|size| (size, ACCEPTANCE_PER_SIZE))
        .collect();
    let mut selected = Vec::new();
    let mut seen = BTreeSet::new();
    for id in pilot::FROZEN_PILOT_IDS {
        if seen.insert(*id) {
            selected.push((*id).to_string());
            if let Some(task) = by_id.get(id)
                && let Some(left) = need.get_mut(task.size.as_str())
            {
                *left = left.saturating_sub(1);
            }
        }
    }
    let mut swebench: BTreeMap<&str, Vec<&SuiteTask>> = BTreeMap::new();
    for task in &pack.tasks {
        if task.runtime == harvest::RUNTIME && !seen.contains(task.id.as_str()) {
            swebench.entry(task.size.as_str()).or_default().push(task);
        }
    }
    for size in ["small", "medium", "large"] {
        let mut cands = swebench.remove(size).unwrap_or_default();
        cands.sort_by(|left, right| {
            rank_key(&left.id)
                .cmp(&rank_key(&right.id))
                .then_with(|| left.id.cmp(&right.id))
        });
        let want = need.get(size).copied().unwrap_or(0);
        for task in cands.into_iter().take(want) {
            if seen.insert(task.id.as_str()) {
                selected.push(task.id.clone());
            }
        }
    }
    if selected.len() < ACCEPTANCE_N {
        let mut rest: Vec<&SuiteTask> = pack
            .tasks
            .iter()
            .filter(|task| !seen.contains(task.id.as_str()))
            .collect();
        rest.sort_by(|left, right| {
            rank_key(&left.id)
                .cmp(&rank_key(&right.id))
                .then_with(|| left.id.cmp(&right.id))
        });
        for task in rest {
            if selected.len() >= ACCEPTANCE_N {
                break;
            }
            if seen.insert(task.id.as_str()) {
                selected.push(task.id.clone());
            }
        }
    }
    selected.sort();
    selected
}

pub fn evidence_id_reasons(observed: &BTreeSet<String>) -> Vec<String> {
    let expected: BTreeSet<String> = frozen_ids().into_iter().map(str::to_string).collect();
    let mut reasons = Vec::new();
    if observed.len() != ACCEPTANCE_N || *observed != expected {
        let missing: Vec<_> = expected.difference(observed).cloned().collect();
        let extra: Vec<_> = observed.difference(&expected).cloned().collect();
        reasons.push(format!(
            "evidence ids {} != frozen acceptance set {} (sha256={FROZEN_ACCEPTANCE_SHA256}); missing={} extra={}",
            observed.len(),
            ACCEPTANCE_N,
            missing.len(),
            extra.len()
        ));
    }
    reasons
}

pub fn render_acceptance(pack: &SuitePack) -> anyhow::Result<String> {
    let ids = select_acceptance(pack)?;
    let mut sizes: BTreeMap<&str, u32> = BTreeMap::new();
    let mut by_id: BTreeMap<&str, &SuiteTask> = BTreeMap::new();
    for task in &pack.tasks {
        by_id.insert(task.id.as_str(), task);
    }
    let mut n_file = 0u32;
    for id in &ids {
        if let Some(task) = by_id.get(id.as_str()) {
            *sizes.entry(task.size.as_str()).or_default() += 1;
            if task.runtime != harvest::RUNTIME {
                n_file += 1;
            }
        }
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{ACCEPTANCE_SCHEMA} frozen=true n={}/{ACCEPTANCE_N} sha256={}\n",
        ids.len(),
        FROZEN_ACCEPTANCE_SHA256
    ));
    out.push_str(
        "exact acceptance ids; gate requires this set, not any >=300 subset of the 509 pack\n",
    );
    out.push_str(&format!(
        "pilot subset=true file={n_file} sizes small/medium/large={}/{}/{} (large capped by SWE-bench Verified)\n",
        sizes.get("small").copied().unwrap_or(0),
        sizes.get("medium").copied().unwrap_or(0),
        sizes.get("large").copied().unwrap_or(0),
    ));
    for id in &ids {
        out.push_str(&format!("  {id}\n"));
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_acceptance_matches_pack_and_contains_pilot() {
        let pack = crate::suite::load_pack().unwrap();
        let computed = select_acceptance_ids(&pack);
        assert_eq!(computed.len(), ACCEPTANCE_N);
        assert_eq!(ids_sha256(&computed), FROZEN_ACCEPTANCE_SHA256);
        let frozen: Vec<String> = frozen_ids().into_iter().map(str::to_string).collect();
        assert_eq!(computed, frozen);
        let set: BTreeSet<_> = computed.iter().cloned().collect();
        for id in pilot::FROZEN_PILOT_IDS {
            assert!(set.contains(*id), "pilot id {id} missing from acceptance");
        }
        let selected = select_acceptance(&pack).unwrap();
        assert_eq!(selected.len(), ACCEPTANCE_N);
        let mut sizes: BTreeMap<&str, u32> = BTreeMap::new();
        for task in &pack.tasks {
            if set.contains(&task.id) {
                *sizes.entry(task.size.as_str()).or_default() += 1;
            }
        }
        assert_eq!(sizes.get("large").copied(), Some(46));
        assert_eq!(sizes.values().sum::<u32>(), ACCEPTANCE_N as u32);
    }
}
