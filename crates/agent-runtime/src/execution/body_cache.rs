//! Current-turn protocol body cache.
//!
//! 目标群体：fs.read 正文已被 turn checkpoint 截掉、身份头还在、模型
//! 只好原样重读的那批调用（实测 motive=protocol_checkpoint_body_missing）。
//! 缓存把最近几份正文留在 ActiveTurn 内存里，组装下一轮请求时按严格
//! 条件回注，省掉一个模型轮。
//!
//! 边界：条目不进 Context 引擎、不被 admit、不落盘——它只是
//! `ModelInput` 组装时的一次性输入，随轮结束消失。Known mutation 使对
//! 应 path 失效；Unknown mutation 使全部失效（保守）。
//!
//! 覆盖真值：条目保留 `path@revision` 之外的**真实暴露范围与完整性**
//! （F01）。回注只发生在本轮真正进入请求的那些行上，所以「缓存里存在
//! 某个片段」永远不等于「整个文件已经可见」——`path@digest` 命中只
//! 说明身份已知，不构成整文件覆盖证明。

use std::collections::VecDeque;

use agent_contracts::FileBodyWindow;

/// 最多缓存的正文件数。
pub(crate) const MAX_PROTOCOL_BODIES: usize = 4;
/// 单份正文的最大字节数；超限不缓存（长正文本来就该走 artifact）。
pub(crate) const MAX_PROTOCOL_BODY_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtocolBodyEntry {
    path: String,
    digest: String,
    body: String,
    /// 这条正文实际暴露的范围与完整性；`None` 表示来源没有给出可信
    /// 窗口（未知范围），此时它不能作为任何覆盖证明。
    window: Option<FileBodyWindow>,
    /// Unknown mutation 后置 true：字节保留但不再视为当前可用。
    dormant: bool,
}

/// record 的结果：入账 / 超限拒收 / 空值忽略。超限计入
/// oversize 账目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BodyRecordOutcome {
    Stored,
    Oversize,
    Empty,
}

/// 自上一条账目以来的增量计数。actor 在每次模型输入
/// 组装后 drain 一次，以 `ProtocolBodyCacheStats` 事件出账，报告可以
/// 从事件流独立验证命中率，而不是靠推断。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProtocolBodyCacheDeltas {
    /// 物理丢弃的条目数（Known mutation 触碰对应 path、或 LRU 挤出）。
    pub invalidated: u64,
    /// 因 Unknown footprint 被挂起（休眠保留字节）的条数。身份经
    /// BeforeModel 重验证恢复 Fresh 后可再次回注——CachedBytesPresent
    /// ≠ BodyCurrentlyTrusted。
    pub suspended: u64,
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
    ///
    /// `window` 是这份正文实际暴露的范围与完整性（来自 fs.read 的可信
    /// metadata）。`None` 表示范围未知；未知范围不得被当作整文件覆盖，
    /// 但仍可回注正文（回注的资格由身份与 Fresh 事实决定，不由覆盖证明
    /// 决定）。
    pub(crate) fn record(
        &mut self,
        path: &str,
        digest: &str,
        body: &str,
        window: Option<FileBodyWindow>,
    ) -> BodyRecordOutcome {
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
            // A window that names a different path/revision than the entry
            // itself cannot describe this body; drop it rather than let a
            // mismatched interval prove coverage for the wrong identity.
            window: window.filter(|window| {
                window.path == path
                    && window
                        .revision
                        .as_deref()
                        .is_none_or(|revision| revision == digest)
            }),
            dormant: false,
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

    /// Unknown mutation：无法知道改了什么，全部条目休眠而非物理删除
    /// （Unknown ≠ False 的延伸）。之后 BeforeModel 的
    /// 本地重验证若证明 path@digest 未变，条目恢复可回注；确实变了
    /// 则永远过不了身份门，由 LRU 自然淘汰。返回挂起条数。
    pub(crate) fn suspend_all(&mut self) -> usize {
        let mut suspended: usize = 0;
        for entry in &mut self.entries {
            if !entry.dormant {
                entry.dormant = true;
                suspended += 1;
            }
        }
        self.pending.suspended += suspended as u64;
        suspended
    }

    /// 取走自上一条账目以来的增量计数。
    pub(crate) fn drain_deltas(&mut self) -> ProtocolBodyCacheDeltas {
        std::mem::take(&mut self.pending)
    }

    /// 当前可回注行：(identity, body, window)，最新在前。Active 条目直接
    /// 可选；Dormant 条目只有在事实表里同 path@digest 重新成为 Fresh
    /// （BeforeModel 重验证通过）时才恢复资格。调用方仍会核对
    /// checkpoint 是否真的截掉了正文。
    ///
    /// `window` 保留条目真实的暴露范围与完整性：回注时用它证明覆盖，
    /// 而不是假定整文件可见（F01）。
    pub(crate) fn eligible_rows(
        &self,
        fresh_identities: &[(String, String)],
    ) -> Vec<crate::prompt::ProtocolBodyRow> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| {
                !entry.dormant
                    || fresh_identities
                        .iter()
                        .any(|(path, digest)| path == &entry.path && digest == &entry.digest)
            })
            .map(|entry| crate::prompt::ProtocolBodyRow {
                identity: format!("{}@{}", entry.path, entry.digest),
                body: entry.body.clone(),
                window: entry.window.clone(),
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

    fn window(path: &str, revision: &str, start: u32, end: u32) -> FileBodyWindow {
        FileBodyWindow {
            path: path.into(),
            revision: Some(revision.into()),
            start_line: Some(start),
            end_line: Some(end),
            covers_file: false,
            complete: true,
        }
    }

    #[test]
    fn record_is_bounded_and_lru_evicts_oldest() {
        let mut cache = ProtocolBodyCache::default();
        for index in 0..(MAX_PROTOCOL_BODIES + 2) {
            cache.record(&format!("src/f{index}.rs"), "r1", "body", None);
        }
        assert_eq!(cache.len(), MAX_PROTOCOL_BODIES);
        // 最老的两个被挤出；最新的在 rows 首位。
        assert!(cache.lookup("src/f0.rs", "r1").is_none());
        let rows = cache.eligible_rows(&[]);
        assert!(rows[0].identity.starts_with("src/f5.rs@"));
    }

    #[test]
    fn oversized_bodies_are_never_cached() {
        let mut cache = ProtocolBodyCache::default();
        let big = "x".repeat(MAX_PROTOCOL_BODY_BYTES + 1);
        assert_eq!(
            cache.record("src/big.rs", "r1", &big, None),
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
        cache.record("src/a.rs", "r1", "a-body", None);
        cache.record("src/b.rs", "r1", "b-body", None);
        // Known mutation 只物理丢弃对应 path。
        assert_eq!(cache.invalidate_path("src/a.rs"), 1);
        assert!(cache.lookup("src/a.rs", "r1").is_none());
        assert!(cache.lookup("src/b.rs", "r1").is_some());
        // Unknown mutation 休眠全部条目：字节保留、资格冻结。
        assert_eq!(cache.suspend_all(), 1);
        let fresh = vec![("src/b.rs".to_string(), "r1".to_string())];
        assert!(cache.eligible_rows(&[]).is_empty(), "dormant without proof");
        let rows = cache.eligible_rows(&fresh);
        assert_eq!(rows.len(), 1, "revalidated identity restores eligibility");
        assert!(rows[0].identity.starts_with("src/b.rs@"));
        assert_eq!(
            cache.drain_deltas(),
            ProtocolBodyCacheDeltas {
                invalidated: 1,
                suspended: 1,
                oversize: 0
            }
        );
    }

    #[test]
    fn dormant_entry_stays_ineligible_when_the_identity_changed() {
        let mut cache = ProtocolBodyCache::default();
        cache.record("src/a.rs", "r1", "a-body", None);
        cache.suspend_all();
        // 重验证发现内容已变（Fresh 但 digest 不同）：身份门不过，
        // 休眠条目永不回注，等 LRU 淘汰。
        let changed = vec![("src/a.rs".to_string(), "r2".to_string())];
        assert!(cache.eligible_rows(&changed).is_empty());
        // 新观察覆盖同 path：恢复为 Active 且换新身份。
        cache.record("src/a.rs", "r2", "new-body", None);
        assert_eq!(cache.eligible_rows(&[]).len(), 1);
    }

    #[test]
    fn eligible_rows_carry_the_recorded_window_verbatim() {
        // F01: 缓存行不再把范围压成 `path@digest`——回注时组装器需要的
        // 是这条正文真实暴露的区间，而不是「整文件已可见」的假定。
        let mut cache = ProtocolBodyCache::default();
        cache.record(
            "src/a.rs",
            "r1",
            "lines 101..200",
            Some(window("src/a.rs", "r1", 101, 200)),
        );
        let rows = cache.eligible_rows(&[]);
        assert_eq!(rows.len(), 1);
        let recorded = rows[0].window.as_ref().expect("window preserved");
        assert_eq!(recorded.start_line, Some(101));
        assert_eq!(recorded.end_line, Some(200));
        assert!(!recorded.covers_file);
        assert!(recorded.complete);
    }

    #[test]
    fn a_window_for_another_identity_is_not_attached() {
        // 范围元数据若与条目身份不符（换版本、换 path），宁可丢窗口也
        // 不能让它替错误的身份作证。
        let mut cache = ProtocolBodyCache::default();
        cache.record(
            "src/a.rs",
            "r1",
            "body",
            Some(window("src/a.rs", "r2", 1, 100)),
        );
        assert!(cache.eligible_rows(&[])[0].window.is_none());
        cache.record(
            "src/a.rs",
            "r1",
            "body",
            Some(window("src/b.rs", "r1", 1, 100)),
        );
        assert!(cache.eligible_rows(&[])[0].window.is_none());
    }

    #[test]
    fn unknown_window_stays_unknown_instead_of_becoming_whole_file() {
        // 来源没给出可信范围时，条目仍可回注正文（资格由身份与 Fresh
        // 事实决定），但绝不能因此生成一个 covers_file 覆盖声明。
        let mut cache = ProtocolBodyCache::default();
        cache.record("src/a.rs", "r1", "body", None);
        let rows = cache.eligible_rows(&[]);
        assert_eq!(rows.len(), 1, "body remains eligible for rehydration");
        assert!(
            rows[0].window.is_none(),
            "unknown range must not be upgraded into a coverage proof"
        );
    }
}
