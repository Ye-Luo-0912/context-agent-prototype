//! Graded retrieval access signals.
//!
//! Authority metadata stays on the body. This module is the single writer
//! for retrieval stamps so search / inspect / fetch / ack cannot each
//! invent a different recency clock. Weights are explicit; there is no
//! learned scoring.

use std::hash::{Hash, Hasher};

use agent_contracts::{AccessSignal, ContextItemId, ContextSearchQuery, ExternalizedContext};

use crate::engine::State;
use crate::store::externally_retrievable;

/// 每次 `context.search` 最多给 Cold 老化锚点一次机会的命中数。
pub(crate) const SEARCH_REINFORCE_MAX_PER_CALL: usize = 8;
/// 同一条目在更强信号出现前，search 最多刷新几次 `last_access_gc_epoch`。
/// 1 = 保留 的一次延迟，同时禁止 search 循环把条目钉死。
pub(crate) const SEARCH_REINFORCE_SATURATION: u32 = 1;
/// 同一检索指纹每个用户回合最多强化一次。limit 不参与指纹。
pub(crate) const SEARCH_IDENTICAL_QUERY_BUDGET: u32 = 1;

/// 消费确认：最强在线信号。驻留条目记 turn/count；外部条目还锚定 GC 世代。
pub(crate) fn stamp_consumed(
    state: &mut State,
    item_id: ContextItemId,
    now_tick: u64,
    turn: u64,
    gc_epoch: u64,
) -> bool {
    let applied = stamp(
        state,
        item_id,
        AccessSignal::ConsumptionAck,
        now_tick,
        Some(turn),
        Some(gc_epoch),
    );
    if applied {
        crate::reactivation::mark_consumed(state, item_id);
    }
    applied
}

/// inspect / fetch 的故意读取。弱于 ack，强于 search；从不增加
/// `access_count`（那是消费确认的特权）。
pub(crate) fn stamp_read(state: &mut State, item_id: ContextItemId, signal: AccessSignal) -> bool {
    debug_assert!(
        matches!(signal, AccessSignal::Inspect | AccessSignal::Fetch),
        "stamp_read is only for inspect/fetch"
    );
    let now_tick = state.event_seq;
    let gc_epoch = state.gc_epoch;
    stamp(state, item_id, signal, now_tick, None, Some(gc_epoch))
}

/// search 命中：最弱。相同查询本回合预算用尽则整次不写；单条目同一
/// `event_seq` 只写一次；饱和后只动 ranking 时钟，不再推迟 Cold 老化。
pub(crate) fn reinforce_search_hits(
    state: &mut State,
    hits: &[ExternalizedContext],
    query: &ContextSearchQuery,
) {
    if hits.is_empty() {
        return;
    }
    let fingerprint = query_fingerprint(query);
    let used = state
        .search_query_stamps_this_turn
        .get(&fingerprint)
        .copied()
        .unwrap_or(0);
    if used >= SEARCH_IDENTICAL_QUERY_BUDGET {
        return;
    }
    state
        .search_query_stamps_this_turn
        .insert(fingerprint, used.saturating_add(1));

    let now_tick = state.event_seq;
    let gc_epoch = state.gc_epoch;
    for hit in hits.iter().take(SEARCH_REINFORCE_MAX_PER_CALL) {
        apply_search_hit(state, hit.item_id, now_tick, gc_epoch);
    }
}

fn stamp(
    state: &mut State,
    item_id: ContextItemId,
    signal: AccessSignal,
    now_tick: u64,
    turn: Option<u64>,
    gc_epoch: Option<u64>,
) -> bool {
    if state.items.indexes().get(item_id).is_some() {
        {
            let index = state.items.indexes().get(item_id).expect("index present");
            let item = &mut state.items.items_mut()[index];
            item.last_access_tick = now_tick;
            if let Some(turn) = turn {
                item.last_access_turn = turn;
                item.last_selected_turn = turn;
                item.access_count = item.access_count.saturating_add(1);
            }
        }
        bump_access(state, signal);
        return true;
    }
    if state.eviction_buffer.iter().any(|item| item.id == item_id) {
        {
            let item = state
                .eviction_buffer
                .iter_mut()
                .find(|item| item.id == item_id)
                .expect("buffer item present");
            item.last_access_tick = now_tick;
            if let Some(turn) = turn {
                item.last_access_turn = turn;
                item.last_selected_turn = turn;
                item.access_count = item.access_count.saturating_add(1);
            }
        }
        bump_access(state, signal);
        return true;
    }
    let applied = {
        let Some(entry) = state.external.get_mut(item_id) else {
            return false;
        };
        if !externally_retrievable(entry) {
            return false;
        }
        if signal.rank() < entry.last_access_signal.rank() {
            // 弱信号不得覆盖更强的时钟/等级；调用方仍可读取当前描述符。
            return false;
        }
        entry.last_access_tick = now_tick;
        entry.last_access_signal = signal;
        if let Some(gc_epoch) = gc_epoch {
            entry.last_access_gc_epoch = Some(gc_epoch);
        }
        if let Some(turn) = turn {
            entry.last_access_turn = turn;
            entry.last_selected_turn = turn;
            entry.access_count = entry.access_count.saturating_add(1);
        }
        if signal.rank() > AccessSignal::SearchHit.rank() {
            entry.search_reinforce_count = 0;
        }
        true
    };
    if applied {
        bump_access(state, signal);
    }
    applied
}

fn bump_access(state: &mut State, signal: AccessSignal) {
    match signal {
        AccessSignal::Inspect => {
            state.access_inspects = state.access_inspects.saturating_add(1);
        }
        AccessSignal::Fetch => {
            state.access_fetches = state.access_fetches.saturating_add(1);
        }
        AccessSignal::ConsumptionAck => {
            state.access_consumption_acks = state.access_consumption_acks.saturating_add(1);
        }
        AccessSignal::SearchHit => {
            state.access_search_hits = state.access_search_hits.saturating_add(1);
        }
        AccessSignal::Admit => {
            state.access_admits = state.access_admits.saturating_add(1);
        }
        AccessSignal::None => {}
    }
}

fn apply_search_hit(state: &mut State, item_id: ContextItemId, now_tick: u64, gc_epoch: u64) {
    if state.external.get(item_id).is_some() {
        let stamped = {
            let Some(entry) = state.external.get_mut(item_id) else {
                return;
            };
            if !externally_retrievable(entry) {
                return;
            }
            if entry.last_access_signal.rank() > AccessSignal::SearchHit.rank() {
                return;
            }
            // 同一 event_seq 内 search 已写过：冷却，避免同一次检索循环连刷。
            if entry.last_access_signal == AccessSignal::SearchHit
                && entry.last_access_tick == now_tick
            {
                return;
            }
            entry.last_access_tick = now_tick;
            entry.last_access_signal = AccessSignal::SearchHit;
            if entry.search_reinforce_count < SEARCH_REINFORCE_SATURATION {
                entry.last_access_gc_epoch = Some(gc_epoch);
                entry.search_reinforce_count = entry.search_reinforce_count.saturating_add(1);
            }
            true
        };
        if stamped {
            bump_access(state, AccessSignal::SearchHit);
        }
        return;
    }
    // Resident/Warm：最弱戳，不碰 Cold 老化世代。
    stamp(
        state,
        item_id,
        AccessSignal::SearchHit,
        now_tick,
        None,
        None,
    );
}

fn query_fingerprint(query: &ContextSearchQuery) -> u64 {
    // 显式 FNV-1a：预算表活在同一进程的 State 里，但不能依赖
    // DefaultHasher 的随机种子（每次 new() 可能换钥）。
    let mut hasher = Fnv64::new();
    query.query.to_lowercase().hash(&mut hasher);
    query.kind.hash(&mut hasher);
    query.scope.hash(&mut hasher);
    query.task_id.hash(&mut hasher);
    query
        .label
        .as_deref()
        .map(str::to_lowercase)
        .hash(&mut hasher);
    hasher.finish()
}

struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }
}

impl Hasher for Fnv64 {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = self.0.wrapping_mul(0x100_0000_01b3) ^ u64::from(byte);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::query_fingerprint;
    use agent_contracts::{AccessSignal, ContextKind, ContextSearchQuery};

    #[test]
    fn query_fingerprint_ignores_limit_and_is_case_insensitive() {
        let a = ContextSearchQuery::new("AuthService", 8);
        let mut b = ContextSearchQuery::new("authservice", 32);
        b.kind = Some(ContextKind::Note);
        assert_ne!(query_fingerprint(&a), query_fingerprint(&b));
        let c = ContextSearchQuery::new("AUTHSERVICE", 1);
        assert_eq!(query_fingerprint(&a), query_fingerprint(&c));
    }

    #[test]
    fn access_signal_ranks_are_strictly_graded() {
        assert!(AccessSignal::SearchHit.rank() < AccessSignal::Inspect.rank());
        assert!(AccessSignal::Inspect.rank() < AccessSignal::Fetch.rank());
        assert!(AccessSignal::Fetch.rank() < AccessSignal::Admit.rank());
        assert!(AccessSignal::Admit.rank() < AccessSignal::ConsumptionAck.rank());
    }
}
