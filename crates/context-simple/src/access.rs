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

/// 上限：已验证的 pending 冷消费记录环。记录在对应卡片版本水化时落账并
/// 离环；超过上限丢最旧一行。聚合 ack 计数不受丢行影响，上限只约束
/// checkpoint 里的逐项事实大小。
pub(crate) const PENDING_COLD_CONSUMED_CAP: usize = 128;

/// 一次已验证的 pending 冷卡片消费（Q1）：模型在 `tick`/`turn` 确实看到了
/// `(item_id, card_hash)` 这张卡片版本的正文。持久化——checkpoint/restore
/// 不丢；当这张卡片版本水化时，记录把真实的 tick/turn 盖到新驻留的条目上
/// （真实事件的延迟记账，不是伪造强化）：热度、admit、热集与 Cold 老化的
/// GC 世代锚都不因此移动。卡片版本绑定与 owner 校验同源：换版本的卡片不
/// 落这份账。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingColdConsumed {
    pub(crate) item_id: ContextItemId,
    pub(crate) card_hash: String,
    pub(crate) turn: u64,
    pub(crate) tick: u64,
}

/// 已验证 pending 冷 owner 的消费结算：聚合 ack 计数前进（消费确实发生），
/// 并写入一条有界、持久化的 `(id, card hash, turn, tick)` 记录，等该卡片
/// 版本下次水化时落真实的访问戳。条目本身不在内存——驻留、热度、pin、
/// 水化都不动。返回 true（事实已记录），供 debug 断言与提交语义使用。
pub(crate) fn stamp_pending_cold_consumed(
    state: &mut State,
    item_id: ContextItemId,
    card_hash: String,
    now_tick: u64,
    turn: u64,
) -> bool {
    bump_access(state, AccessSignal::ConsumptionAck);
    crate::reactivation::mark_consumed(state, item_id);
    prune_pending_cold_consumed(state);
    state.pending_cold_consumed.push(PendingColdConsumed {
        item_id,
        card_hash,
        turn,
        tick: now_tick,
    });
    if state.pending_cold_consumed.len() > PENDING_COLD_CONSUMED_CAP {
        state.pending_cold_consumed.remove(0);
    }
    true
}

/// 记录的落账出口：`installed` 是刚装进 external 表的 `(id, card hash)`。
/// 版本一致的记录把真实 tick/turn 盖到新条目上（ConsumptionAck 是最强
/// 信号，必落），随后离环；不落 GC 世代锚——消费发生时条目并不驻留，
/// 不授予 Cold 老化延期。卡片已消失的版本由下一次写入时的惰性清理移除。
pub(crate) fn land_pending_cold_consumptions(
    state: &mut State,
    installed: &[(ContextItemId, String)],
) {
    if state.pending_cold_consumed.is_empty() {
        return;
    }
    let mut landed: Vec<PendingColdConsumed> = Vec::new();
    {
        let keys: std::collections::HashSet<(&ContextItemId, &String)> =
            installed.iter().map(|(id, hash)| (id, hash)).collect();
        state.pending_cold_consumed.retain(|record| {
            if keys.contains(&(&record.item_id, &record.card_hash)) {
                landed.push(record.clone());
                return false;
            }
            true
        });
    }
    for record in landed {
        let _ = state.external.stamp_access(
            record.item_id,
            AccessSignal::ConsumptionAck,
            record.tick,
            None,
            Some(record.turn),
            None,
        );
    }
}

/// 丢弃既不对应任何 pending 定位行、也无同版本已记录卡片的记录：它们的
/// 版本永远不可能再水化，留在环里只会挤出还可能落账的事实。
fn prune_pending_cold_consumed(state: &mut State) {
    let live_rows: std::collections::HashSet<(ContextItemId, String)> =
        state.pending_external_cards.iter().cloned().collect();
    state.pending_cold_consumed.retain(|record| {
        live_rows.contains(&(record.item_id, record.card_hash.clone()))
            || state
                .external
                .card_hash(record.item_id)
                .is_some_and(|hash| hash == record.card_hash)
    });
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
    // Retry-list items are stamped exactly like warm-buffer bodies: their
    // content is still in memory and the stamp flows into the external
    // entry when the retried write finally lands.
    if state
        .pending_externalize_retry
        .iter()
        .any(|item| item.id == item_id)
    {
        {
            let item = state
                .pending_externalize_retry
                .iter_mut()
                .find(|item| item.id == item_id)
                .expect("retry item present");
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
    // S3: the stamp goes through the named claim-keeping map operation —
    // access stamps are not card-relevant, and killing the claim here would
    // make every fetched entry undemotable (the hot cap could never settle
    // after a fetch).
    let applied = state
        .external
        .stamp_access(item_id, signal, now_tick, gc_epoch, turn, None);
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
    // S3: guards read through the immutable lookup; the write goes through
    // the named claim-keeping stamp (search-hit reinforcement is also not
    // card-relevant metadata).
    if let Some(entry) = state.external.get(item_id) {
        if !externally_retrievable(entry) {
            return;
        }
        if entry.last_access_signal.rank() > AccessSignal::SearchHit.rank() {
            return;
        }
        // 同一 event_seq 内 search 已写过：冷却，避免同一次检索循环连刷。
        if entry.last_access_signal == AccessSignal::SearchHit && entry.last_access_tick == now_tick
        {
            return;
        }
        let reinforce =
            (entry.search_reinforce_count < SEARCH_REINFORCE_SATURATION).then_some(gc_epoch);
        let stamped = state.external.stamp_access(
            item_id,
            AccessSignal::SearchHit,
            now_tick,
            None,
            None,
            reinforce,
        );
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
