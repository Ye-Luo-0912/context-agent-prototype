# W2 回执 — 原子搜索结果与 continuation 生命周期（V3＋V4＋V5）

工单：`docs/NEXT_TASKS.md` 第四批 W2 节；缺陷细节：本目录 `REVIEW.md` 的
V3/V4/V5 节。执行日期：2026-09-16。基线：HEAD `b6ffc514` 之上的工作树
（本切片主体由前一位被取消的 Agent 留下、未提交；本回执如实区分「接手时已在
树里」与「本次补完/新写」）。本回执只记录本切片实际发生的事。

## 接手盘点（实施前）

前一位 W2 Agent 被取消时，改动已在工作树且 `cargo check` 通过，但测试未全部
编译、集成测试从未跑绿、无红-first 证据可考。逐文件盘点：

已在树里（半成品，核对后保留）：
- `contracts/context.rs`：`ContextSearchResult { hits, coverage, observation }`
  原子契约类型（serde）、`ContextSearchCoverageStop::Unknown` 变体、
  `ContextEngine::search_external_report` trait 方法（默认实现组合旧方法）。
- `context-simple/src/engine.rs`：`search_with_continuation` →
  `search_report_with_continuation`（返回原子结果）；`finish_search_report`；
  `record_search_coverage(resumed)` 的 fresh/resume 分裂＋`merge_covered_dedup`
  去重＋链级上限 `search_continuation_max_covered_ids`（超界 → typed Budget
  停止、无 token、释放状态）；`continuation_serial`/`continuation_epoch` 两个
  进程生命周期单调原子量；token 形如 `cold-window-e{epoch}-n{serial}`；
  `rotate_search_window` 返回 `Option`（token/query/epoch 三重验证）；
  `restore` 安装成功后清 slot＋bump epoch；`#[cfg(test)]
  search_continuation_probe`。
- `context-contextcore`：wire 新 op `SearchExternalReport`（fresh 不序列化
  continuation 字段）＋pong 顶层 `features` 表＋`FEATURE_CONTEXT_SEARCH_REPORT`
  ＝ `context-search-report.v1`；adapter 的 `last_report` 兼容镜像、
  `search_external_report`（未协商 → 显式 Unsupported）、诚实化后的
  `last_search_coverage`（无事实 → Unknown）；`--cold-paging` 三元组参数。
- `agent-context-service`：`SearchExternalReport` handler 转发原子结果；
  ping 响应附带 feature 表；`--cold-paging` 解析进 `build_engine`。
- `agent-core/src/kernel/mod.rs`：context.search 工具改从
  `search_external_report` 的返回取 coverage/observation（旁路不再参与
  service 链），model-visible 正文与 metadata 携带同一 pass 的事实。
- 新测试文件 `context-simple/src/tests/search_continuation.rs`（7 条）与
  `context-contextcore/tests/service.rs` 追加段（4 条）。

核对结论：逻辑主体方向正确、与 W1 改动（同文件的 `PendingIdOutcome`、
`resolve_required_cold_refs`、`probe_pending_scope_references`）互不重叠；
协商链依赖的 `ProcessHost` offered/negotiated features 基础设施为既有实现，
未改动。缺的只有三件事：service.rs 追加测试**编译不过**（三处）、fixture
搜索词不可命中（两测试红）、全部红-first 证据缺失。

## 本次补完/新写

1. **service.rs 三处编译错误**（半成品未收尾，按该文件既有惯用法补完）：
   - `a_fresh_boundary_never_self_certifies_complete_coverage` 改为绑定具体
     `ContextServiceAdapter`（共享 `connect()` 助手把 shutdown 抹成 trait
     object）；parity 测试去掉 `Arc<dyn ContextEngine>` 遮蔽、保持具体类型
     直接调用＋结尾 shutdown；`local_replay_token` 参数改 `&dyn ContextEngine`。
   - `service_facts.1` 改 `service_facts`（原写法把 fact 元组错索引成
     `usize` 与四元组比较）。
2. **fixture 搜索面修正**：parity/legacy 夹具原把 needle 放进 ToolObservation
   正文——raw evidence kind 的正文按设计不是搜索面（`kind_has_searchable_body`
   为 false），首 pass 零命中＋未读完 → fail-closed Err，两测试失败。修正为
   把 needle 盖进 `metadata.path`（`w2-needle.rs`，索引化实体，与文件内既有
   sentinel 测试同一搜索面），查询词同步改为 `w2-needle.rs`。语义不变：
   仍是「同一条历史、双引擎、四场景」parity。
3. **`agent-context-service/src/main.rs` 一处 clippy** `manual_contains`
   （`iter().any(== 0)` → `contains(&0)`）。
4. **红-first 证据**（半成品无法证明红过，按下述方式补齐）。
5. `cargo fmt` 五 crate（机械，全部落在半成品引入的行上）。

## 机制（最终形状）

### V3 — 一次搜索原子返回

- 契约：`ContextSearchResult`（新类型，`Serialize/Deserialize`；新增类型对旧
  消费方零影响）一次携带 hits＋coverage＋observation；`hits` 与 `coverage`
  恒属同一 pass。空命中压未读区间仍 fail-closed（Err），`Ok` 且空 hits 即
  「已证完覆盖下的零匹配」。
- 引擎：`search_report_with_continuation` 是唯一 pass 实现；
  `search_external`/`search_external_continuation` 降为取 `.hits` 的薄包装；
  trait 新默认方法 `search_external_report(query, Option<token>)`——进程内
  引擎可继续用默认组合，**进程边界适配器不得用旁路满足它**。
- 服务链：adapter `search_external_report` → `ServiceOp::SearchExternalReport`
  → service handler → 引擎，真实 coverage 原样过线；Core（kernel/mod.rs 的
  context.search 工具）从原子结果取事实，`last_search_*` 旁路不在 service
  链上（方法保留，语义诚实化：未协商/未搜索 → `Unknown`，不再默认 complete）。
- 协议迁移：沿既有握手协商——adapter 在 ping `offered_features` 里提供
  `context-search-report.v1`；service pong 顶层 `features` 表声明它；
  `ProcessHost` 交集为 negotiated。旧服务 pong 无该字段 → 未协商 →
  adapter 明确返回 Unsupported 错误（续查转发同理），绝不静默降级为
  「普通完整结果」。

### V4 — fresh/resume 分裂与链级边界

- fresh（无 token，或 token 未通过旋转验证）：开始新遍历，不继承任何旧
  covered 集合；covered 恰为本次 pass 看到的 carded hot 窗口。
- resume（验证过的 token）：covered ＝ 去重并集（`merge_covered_dedup`，
  保持首见序）；同一 pass 反复可 slot 内不再线性追加。
- 链级上限：`search_continuation_max_covered_ids`（默认 16,384 ≈ 256 KiB，
  `ContextItemId` 定长 16 字节，条数即字节界）。超界 → 链关闭：状态释放、
  typed `Budget` 停止、无 token；调用方重新 fresh 即可继续，可达性不丢。

### V5 — restore 失效与单调身份

- 成功 restore：清空续查 slot＋bump `continuation_epoch`——旧 token 是对
  已不存在视图的声明，显式失效；拒绝 restore 在安装 State 前返回，既有合法
  链原样不动。
- token 身份：`cold-window-e{epoch}-n{serial}`，serial 进程生命周期单调
  （链完成、换查询、restore 都不回退），epoch 绑定恢复代际——编号不再从
  slot 尾数推导，ABA 复用不可能；旋转时 token/query/epoch 三重验证。
- 陈旧/外来/已消费 token：明确拒绝为续查 → 本次 pass 按 fresh 执行（非
  错误、不混入他遍历状态），重复到达结果确定（幂等面）。

一句话生命周期规则：**fresh 只为自己看过的窗口签发新身份；resume 只延伸
验证过的遍历；restore 之后所有旧 token 是无效声明，重新 fresh 开始。**

## 回归（红-first 证据）

半成品无法证明任何反例红过（无记录可考，按未跑处理）。证明方式：**临时
变异**——把修复段换成旧缺陷行为，跑出红，再从变异前做的字节级备份
（sha256 校验一致）原样恢复；全程不在共享工作树靠记忆恢复（AGENTS.md
RED_CHECK 教训）。每条红都在恢复后的绿代码上复跑确认绿。

`crates/context-simple/src/tests/search_continuation.rs`（7 条全绿）：

1. `repeated_plain_searches_do_not_grow_the_continuation_state`（V4 反例）：
   14 卡固定历史、热窗口 4，连续 12 次普通查询。红（变异 M1＝恢复旧的
   「query_key 相同即无去重追加」）→ `round 1: ... must not grow the retained
   continuation state (8 ids vs first 4)`；绿：逐轮 `covered.len()` 恒 4、
   集合恒等本轮窗口、每轮 token 不同。
2. `a_valid_continuation_advances_without_duplicates_or_missed_pages`（V4 正面）：
   续查链逐页推进、seen 单调不减、最终 `seen == 全历史`、去重集合不超历史。
   （守卫性用例，无需红证。）
3. `restore_invalidates_stale_continuation_tokens`（V5 反例）：V1 视图两步
   遍历后原位恢复 V0。红（M3＝删掉 restore 时的清 slot＋bump epoch）→
   `a successful in-place restore must clear the continuation state`；绿：
   恢复后 slot 为空，旧 token 到达按 fresh 执行（只覆盖自己窗口，不把 V0
   未搜索内容当已覆盖），新身份与旧两代 token 均不同。
4. `a_completed_chains_tokens_are_never_reused_by_a_new_chain`（V5 ABA 反例）：
   链 A 走完查询 alpha、链 B 新查询 beta。红（M4＝token 退回 slot 尾数推导）
   → `a new chain must never reuse a completed chain's token identities:
   ["cold-window-2", "cold-window-1", "cold-window-3"]`（与审查描述的 ABA
   逐字吻合）；绿：两链 token 集不相交。
5. `a_walk_past_its_chain_bound_closes_with_an_explicit_stop_and_no_token`
   （V4 链级上限反例）：上限钉在 2*CAP-3，resume 并入第二窗口即越界。红
   （M2＝删掉上限检查）→ `the chain bound surfaces as the typed items-budget
   stop: ... stop: HotCap, continuation: Some(...)`（越界后照常发 token）；
   绿：越界 pass 照常服务命中，但链关闭（Budget 停止、无 token、状态释放），
   紧随的 fresh 搜索正常开新链。
6. `a_rejected_restore_keeps_the_live_chain_valid`（V5 守卫）：非法
   checkpoint 整体被拒后，既有 token/covered 逐位不变，链继续推进。（守卫，
   无需红证。）
7. `repeating_an_already_consumed_token_degrades_to_a_fresh_walk`（V5 幂等面）：
   已消费 token 重复到达——不报错、不旋转现任窗口、不继承被替换遍历的
   covered，且重复执行结果稳定。（守卫，无需红证。）

`crates/context-contextcore/tests/service.rs`（经 agent-context-service 包
编译运行；W2 段 4 条，全套 20/20 绿）：

8. `a_fresh_boundary_never_self_certifies_complete_coverage`（V3 反例）：未
   搜索的边界不得自证 complete。红（变异 M6＝旁路 `last_search_coverage`
   改回自证 complete，即旧 trait 默认行为）→ `an unsearched boundary has no
   pass to describe: ContextSearchCoverage { complete: true, ... }`；绿：
   `complete == false`＋`stop == Unknown`。
9. `search_report_parity_across_the_service_boundary`（V3 反例集）：同一
   14 卡冷热夹具（restore 批 4/热上限 4/每操作 8，双引擎同参），in-process
   与 service 锁步比对：非空不完整（hits 逐位相等＋facts (false,10,hot-cap,
   true) 相等）、续查到末页（逐 pass hits/facts 相等、同轮完成）、空不完整
   （双侧 fail-closed，错误同提 coverage incomplete＋pending spill page）、
   过期 token（双侧同样降级 fresh、结果相等、旧身份不再匹配）。红（变异
   M7＝`search_report_negotiated` 恒 false，模拟能力缺失）→ 显式
   `context service did not negotiate 'context-search-report.v1': atomic
   search facts (coverage/continuation) are unsupported on this boundary`
   ——旧能力缺失是显式 Unsupported，不是静默 complete；组合证明：M6 下
   同一 facts 断言在 legacy 测试红（complete vs false/10/hot-cap），而
   parity 测试仍绿——**原子通道对旁路污染免疫，正是设计目标**。绿：四场景
   全等。
10. `legacy_search_channel_no_longer_fabricates_complete_coverage`（V3 旁路面）：
    协商建立后 legacy `search_external` 路由经原子 op 并镜像真实事实。
    红（M6）→ `the boundary forwards the real coverage facts: (true, 0,
    Complete) vs (false, 10, HotCap)`；绿：双侧 facts 相等且 continuation
    过线。

不要求红证的守卫/正面用例：2、6、7 及 wire 层两条（op 往返＋pong feature
表编码）。

## 验收命令与结果（2026-09-16 实测，全部最终状态复跑）

- `cargo test -p context-simple` → **425 passed / 0 failed**（含
  search_continuation 7、S3 fixed_budget_closure 7、cold_bounds 2、
  W1 cold_owner_and_required 4）。
- `cargo test -p context-contextcore` → 9 passed / 0 failed（wire 单测）。
- `cargo test -p agent-context-service` → 11 + 0 + **20** passed / 0 failed
  （20 条即跨包编译的 process-boundary 套件，含 W2 四条）。
- `cargo test -p agent-core` → 153 + 12 + 3 passed / 0 failed（kernel 改走
  原子通道后）。
- `cargo test -p agent-contracts` → 183 passed / 0 failed。
- `cargo test -p agent-runtime --test turn` → 146 passed / 0 failed。
- `cargo clippy -p context-simple -p context-contextcore -p
  agent-context-service -p agent-contracts -p agent-core --all-targets` →
  0 warning（修掉半成品留下的 1 处 manual_contains）。
- `cargo fmt -p`（同五 crate）`-- --check` → clean。

注：验证期间并行 W3（`9b176df0`）/W4（`4fa2d8a2`）先后合入 main，上述结果
在包含两者的树上取得；本切片未动 provider-openai / agent-runtime /
agent-compose 任何文件，未 git add/commit/push。

## 改动文件（本 Agent 相对接手时的净变更）

- `crates/context-contextcore/tests/service.rs`：三处编译修复＋fixture 搜索
  面修正（needle 进盖章 path）＋fmt。
- `crates/agent-context-service/src/main.rs`：clippy manual_contains 一处。
- 五 crate `cargo fmt` 机械格式化（全部为半成品引入的未格式化行）。
- 本回执（新文件）。

半成品既有内容（前位 Agent 留下，核对后原样保留）：
`crates/agent-contracts/src/context.rs`、`crates/context-simple/src/engine.rs`
（搜索/续查区域；W1 区域未动）、`crates/context-simple/src/tests/
search_continuation.rs`（新）、`crates/context-simple/src/tests/mod.rs`（注册）、
`crates/context-contextcore/src/{adapter.rs, lib.rs, wire.rs}`、
`crates/context-contextcore/Cargo.toml`、`crates/agent-context-service/src/lib.rs`、
`crates/agent-core/src/kernel/mod.rs`。

## 限制与取舍

- **同 token 同页幂等重试**采「明确拒绝为续查 → 稳定按 fresh 执行」语义
  （审查措辞为「可以支持」，非强制同页重放）：不报错、可重复、不混入不同
  遍历状态；同页重放语义（重发同一页）未做，如需更强的 at-least-once
  分页语义应改为绑定目录快照的页位置游标（审查长期方向），本轮不展开。
- V4 的 covered 集合仍是「遍历累积的 ID 集合」而非目录快照游标——这是
  审查指明的长期形态，本轮交付的是请求级边界（fresh/resume 分裂、去重、
  链级上限、显式超界），不冒充分页游标已完成。
- 估算与边界口径沿用 `HydrationBudget`/热上限既有形状；未新建事件数据库或
  第二套状态权威；`search_continuation_max_covered_ids` 默认 16,384 是保守
  首值，无真实规模数据支撑调优。
- `NEXT_TASKS.md` 的 W2 节未标记关闭：按指示本切片不提交，关闭标注随合并
  提交一起做。
- 未知旧服务在「搜索」之外的能力（如 inspect/fetch）不属 V3 范围，未加
  capability 门。
- Windows 下未遇僵尸 test 进程；与并行 Agent 的 target 锁等待出现过、属正常。
