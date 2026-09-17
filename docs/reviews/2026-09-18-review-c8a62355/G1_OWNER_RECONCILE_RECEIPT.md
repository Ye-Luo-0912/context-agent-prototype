# G1 回执 — 逻辑 owner 驱动 reconcile

提交：`62b20e4b`（基线 `2bf85961`，即审查基线 `c8a62355` ＋ 第十批开工文档）。任务规格：[NEXT_ACTIONS.md](NEXT_ACTIONS.md) G1；缺陷分析：[REVIEW.md](REVIEW.md) 第 2 节。

## 用户动作

固定小热预算下冷历史仍可检索／取回；恢复后 reconcile 不把现存冷 owner 当作孤儿，也不把全部历史拉回热表。

## 实现（生产入口各一处）

- `engine.rs::reconcile_store_protecting`：owner 快照加入 `pending_external_cards` 的 id 集合；adopted orphans 之后走既有 `settle_metadata_residency` 单一结算入口（与 GC 相同路径）。`loaded_owner_count` 改 `pub(crate)` 复用既有定义，不另立权威。
- `store.rs::run_reconcile_io_protecting`：新增 `pending_card_ids` owner-snapshot 参数——blob 保留分支与 card 清扫保留集都使用；未读冷卡片的 blob 以可解释 reason 行保留（"a pending cold card owns the id (metadata unread, not ownerless)"），不重新认领、不删除。
- `store.rs::commit_reconcile`：候选提交前对照全部 owner 位置复核——heap/warm/retry/external map（经 `loaded_owner_count`）＋任意 pending 行；覆盖无锁 IO 窗口内并发 per-id 结算把条目降级为 pending 的情形。只有真正孤儿被接纳。
- 不用 blob 重建状态覆盖已知冷卡片身份；合法孤儿接纳后热目录经既有预算结算降级回预算内（carded 最老条目降级、claim 保留），残余无法修复时以类型化 reason 行＋`reasons_truncated` 记账（`StoreReconcileReport` 在共享 crate，字段扩展留给集成人）。

## 回归（红→绿，先只有测试代码时对未修改生产逻辑运行）

引擎级（`tests/owner_reconcile.rs`，fixture：热上限 1、热 A＋pending B/C、blob/卡片齐全、卡片与 blob 快照分歧——卡片 importance 0.9 vs blob 冻结 0.5）：

- `reconcile_does_not_reclaim_a_pending_cold_owners_blob` — 修复前红：`no logical id may be owned twice (hot {3 ids} vs pending {2 ids})`（即审查反例：热 1→3、pending 仍 2）。修复后：hydration 以 `HydrationStop::HotCap` 停止、每 id 恰一 owner、热∩pending=∅、pending `(id, hash)` 行不变、`fetch_external` 返回**卡片**版本（0.9 非 0.5）。
- `a_true_orphan_is_adopted_through_the_residency_settlement`（正对照）— 真孤儿被接纳（`rebuilt==1`）且热表经结算降至预算内，两个正文均可取回。修复前红：`'the true orphan is adopted' left: 3, right: 1`（pending owner B/C 被一并重新认领）。
- `repeated_reconcile_over_a_fixed_hot_budget_is_idempotent` — 两轮 reconcile owner 集守恒、层间互斥、冷卡片版本保持、第二轮零重建/删除。
- `checkpoint_restore_roundtrip_keeps_cold_bodies_fetchable` — checkpoint→restore→三正文按卡片版本可取回，roundtrip 后 reconcile 不变量保持。

store 级：`reconcile_keeps_a_pending_owned_blob_without_readopting_it`（扫描分支：pending 拥有的 blob 保留＋reason，不进 rebuilt_candidates；无 pending 行的对照轮仍接纳真孤儿）、`commit_reconcile_rejects_candidates_claimed_by_any_owner_location`（pending 行或 heap body 认领的候选被拒，真孤儿被接纳）。

## 已执行验证（Windows，cargo 1.97.1）

- `cargo test -p context-simple`（合树后集成复跑）：**448 passed / 0 failed**（基线 442＋6 新增；含 fmt 后复跑一次）。
- `cargo clippy -p context-simple --all-targets -- -D warnings`：干净。
- `cargo fmt -p context-simple --check`：干净。

## 边界与残余

- 未全历史 hydration、未 pin、未放宽 owner validator；保护根／删除延期行为未回退（既有测试保持绿）。
- 结算只降级 carded 条目：新接纳孤儿（磁盘无卡片）自身不能降级，超预算状态下后续 per-id fetch 可能留下残余超预算热表——GC 式报告给出类型化事实（S3 既有语义，非本片恶化；下一次 checkpoint 卡片化后解锁降级）。
- 扫描快照与提交之间的双并发引擎操作竞态由 commit 复核逻辑覆盖（op_gate 已串行 GC/restore/reconcile；仅 per-id fetch 可交错），无专门竞态测试。
