//! Current-turn protocol body cache (PROTO-EVID-01).
//!
//! 目标群体：fs.read 正文已被 turn checkpoint 截掉、身份头还在、模型
//! 只好原样重读的那批调用（实测 motive=protocol_checkpoint_body_missing）。
//! 缓存把最近几份正文留在 ActiveTurn 内存里，组装下一轮请求时按严格
//! 条件回注，省掉一个模型轮。
//!
//! 边界：条目不进 Context 引擎、不被 admit、不落盘——它只是
//! `ModelInput` 组装时的一次性输入，随轮结束消失。Known mutation 使对
//! 应 path 失效；Unknown mutation 使全部失效（保守）。

use std::collections::VecDeque;

/// 最多缓存的正文件数。
pub(crate) const MAX_PROTOCOL_BODIES: usize = 4;
/// 单份正文的最大字节数；超限不缓存（长正文本来就该走 artifact）。
pub(crate) const MAX_PROTOCOL_BODY_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtocolBodyEntry {
    path: String,
    digest: String,
    body: String,
}

/// record 的结果：入账 / 超限拒收 / 空值忽略。超限计入 PROTO-EVID-02b
/// 的 oversize 账目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyRecordOutcome {
    Stored,
    Oversize,
    Empty,
}

/// 自上一条账目以来的增量计数（PROTO-EVID-02b）。actor 在每次模型输入
/// 组装后 drain 一次，以 `ProtocolBodyCacheStats` 事件出账，报告可以
/// 从事件流独立验证命中率，而不是靠推断。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProtocolBodyCacheDeltas {
    /// 因 Known/Unknown mutation 失效而丢弃的条目数。
    pub invalidated: u64,
    /// 因超过单份正文字节上限被拒绝缓存的正文数。
    pub oversize: u64,
}

/// ActiveTurn 生命周期的小 LRU：key = path，命中要求 digest 一致。
#[derive(Debug, Clone, Default)]
pub(crate) struct ProtocolBodyCache {
    entries: VecDeque<ProtocolBodyEntry>,
    pending: ProtocolBodyCacheDeltas,
}

impl ProtocolBodyCache {
    /// 记录一份成功观察到的正文。同 path 覆盖并移到最新位。
    pub(crate) fn record(&mut self, path: &str, digest: &str, body: &str) -> BodyRecordOutcome {
        if path.is_empty() || digest.is_empty() || body.is_empty() {
            return BodyRecordOutcome::Empty;
        }
        if body.len() > MAX_PROTOCOL_BODY_BYTES {
            self.pending.oversize += 1;
            return BodyRecordOutcome::Oversize;
        }
        self.entries.retain(|entry| entry.path != path);
        self.entries.push_back(ProtocolBodyEntry {
            path: path.to_string(),
            digest: digest.to_string(),
            body: body.to_string(),
        });
        while self.entries.len() > MAX_PROTOCOL_BODIES {
            self.entries.pop_front();
        }
        BodyRecordOutcome::Stored
    }

    /// Known mutation：这个 path 的旧正文不再是当前身份。返回失效条数。
    pub(crate) fn invalidate_path(&mut self, path: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.path != path);
        self.pending.invalidated += (before - self.entries.len()) as u64;
        before - self.entries.len()
    }

    /// Unknown mutation：所有正文都可能过期，全部作废。返回失效条数。
    pub(crate) fn invalidate_all(&mut self) -> usize {
        let removed = self.entries.len();
        self.entries.clear();
        self.pending.invalidated += removed as u64;
        removed
    }

    /// 取走自上一条账目以来的增量计数。
    pub(crate) fn drain_deltas(&mut self) -> ProtocolBodyCacheDeltas {
        std::mem::take(&mut self.pending)
    }

    /// 当前可回注行：(path@digest, body)，最新在前。调用方还要核对
    /// 事实表 Fresh 与 checkpoint 是否真的截掉了正文。
    pub(crate) fn rows(&self) -> Vec<(String, String)> {
        self.entries
            .iter()
            .rev()
            .map(|entry| {
                (
                    format!("{}@{}", entry.path, entry.digest),
                    entry.body.clone(),
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn lookup(&self, path: &str, digest: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.path == path && entry.digest == digest)
            .map(|entry| entry.body.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_bounded_and_lru_evicts_oldest() {
        let mut cache = ProtocolBodyCache::default();
        for index in 0..(MAX_PROTOCOL_BODIES + 2) {
            cache.record(&format!("src/f{index}.rs"), "r1", "body");
        }
        assert_eq!(cache.len(), MAX_PROTOCOL_BODIES);
        // 最老的两个被挤出；最新的在 rows 首位。
        assert!(cache.lookup("src/f0.rs", "r1").is_none());
        let rows = cache.rows();
        assert!(rows[0].0.starts_with("src/f5.rs@"));
    }

    #[test]
    fn oversized_bodies_are_never_cached() {
        let mut cache = ProtocolBodyCache::default();
        let big = "x".repeat(MAX_PROTOCOL_BODY_BYTES + 1);
        assert_eq!(
            cache.record("src/big.rs", "r1", &big),
            BodyRecordOutcome::Oversize
        );
        assert_eq!(cache.len(), 0);
        // 超限拒收计入增量账目，drain 后归零。
        assert_eq!(cache.drain_deltas().oversize, 1);
        assert_eq!(cache.drain_deltas(), ProtocolBodyCacheDeltas::default());
    }

    #[test]
    fn invalidation_rules_match_mutation_footprints() {
        let mut cache = ProtocolBodyCache::default();
        cache.record("src/a.rs", "r1", "a-body");
        cache.record("src/b.rs", "r1", "b-body");
        // Known mutation 只杀对应 path。
        assert_eq!(cache.invalidate_path("src/a.rs"), 1);
        assert!(cache.lookup("src/a.rs", "r1").is_none());
        assert!(cache.lookup("src/b.rs", "r1").is_some());
        // Unknown mutation 全部作废。
        assert_eq!(cache.invalidate_all(), 1);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.drain_deltas().invalidated, 2);
    }
}
