# S3 回执——固定预算冷目录闭环（R3/R4）

- 状态：实现完成，待主会话验收提交（本回执撰写时未 commit，工作树基于 `4ba60b97`）。
- 执行者：S3 工单（NEXT_TASKS.md 第三批），并行于另一 Agent 的 agent-host 工作（本切片未触碰 agent-host 下任何文件）。
- 范围：`agent-contracts`、`context-simple`、`agent-core`、`tool-runtime`、`agent-runtime`（测试）、`agent-eval`（新报告字段的字面量补齐）。

## 现场描述（SHA 前）

基线 `4ba60b97` 上，T4 一期已落地每操作预算（items＋绝对 deadline＋热上限）与类型化 `HydrationOutcome`，但按续审 R3/R4 核实，五个缺陷全部存在：

- (a) `hydrate_within_budget` 只在批间检查 deadline，`hydrate_pending_cards` 的单次卡片读取在 await 点无期限；
- (b) 批 `take` 由条数额度＋entry room 决定，安装前无字节预留，装完才在下轮发现越界；
- (c) per-id `fetch/inspect` 安装后无任何降级，热表随访问无限增长；`demote_overflow` 只在 full GC 提交后调用一次，返回 bare `Vec`，「无可降级项仍超预算」无类型化事实；
- (d) `finish_search_hits` 在 `hydration.complete || !hits.is_empty()` 时返回普通 `Vec`，非空不完整结果的 coverage 无输出通道；`EngineQuery::SearchExternal` 无 continuation 概念；
- (e) `hydrate_all_pending_cards` 名称承诺「读完全部」，与预算化契约不符。

另发现一个 R3 未单列的结构性根因：`ExternalMap::get_mut` 对每次访问戳（fetch/inspect/search 命中强化）都删除 card claim（保守口径：卡片字节不再逐字匹配活元数据），导致**所有被读过的条目都变成不可降级**——即使加上 GC 后降级，读后驻留也无法在固定热上限内收敛。这是 (c) 的前置条件。

## 每项修复的机制

### 统一 metadata-residency 入口（对应 R3 收口方向）

- `context-simple/src/engine.rs`：新增 `settle_metadata_residency(state, config, protect)`——度量（`MetadataResidencyPressure::measure`，与批量 drain 共用同一估算口径）→ `demote_overflow` 把最旧的已卡片化非 pinned 条目换回 pending 目录（行追加队尾、claim 保留）→ 返回残余 `over_entries/over_bytes`（类型化背压）。调用方：per-id 安装（`hydrate_card_for`，protect 刚服务的 id）、full GC 提交后、ingest directive 路径（protect directive 目标）。
- `index/external.rs`：`demote_overflow(max_entries, max_bytes, skip)` 改为返回类型化 `DemoteOutcome { demoted, over_entries, over_bytes }`，新增 `skip` 参数；新增具名操作 `demote_ids(ids)`（搜索续查的窗口轮转用：只降级列表内的已卡片化非 pinned 条目、保留 claim、维护计数/索引/目录标记）与 `carded_hot_ids()`。
- `ContextGcReport` 新增 `#[serde(default)] hot_metadata_backpressure: bool`（沿 `externalize_backpressure` 的既有先例）：GC 后仍超预算且无可降级项时为 true，不是静默成功。

### (a) deadline 约束单次读取

`hydrate_pending_cards(take, budget, skip)` 中每次读取用 `tokio::time::timeout(剩余 deadline, read)` 包裹；到点取消等待、该行保持 pending owner、无半装提交，批结果计 `timed_out`；drain 循环看到 `timed_out > 0` 即以新类型化原因 `HydrationStop::Deadline` 返回。注释如实声明：约束的是调用等待与状态结算，不宣称物理取消底层 I/O。

### (b) 按剩余字节预留

同一批结算阶段（state 锁内）：`byte_room = hot_max_bytes − 当前估算`，逐条安装前按 `entry_metadata_bytes_estimate` 预留（entry room 同步递减）；装不下的条目保持 pending 可寻址行（计 `oversized`），不为守 cap 丢元数据。估算口径写成注释不变量：与 map 级 `metadata_bytes_estimate` 同一权重（256B/条 + 64B/依赖 + uri/summary/entity 长度），约束估算元数据足迹、非 RSS。

### (c) per-id 驻留受控 + 背压类型化

`hydrate_card_for` 安装后走 `settle_metadata_residency`（protect 自身）：固定上限下顺序 fetch/inspect 形成热↔冷换页窗口；只有 pinned/无 claim 时才如实背压（GC 报告字段/单元级 `DemoteOutcome`）。**前置修复**：新增 `ExternalMap::stamp_access`（具名、保 claim 的访问戳写入），`access.rs` 的 `stamp()` 外置分支与 `apply_search_hit` 改走它——访问戳不再是卡片相关元数据（卡片重装可能带旧时间戳，与 restore 接受的快照陈旧同类），卡片相关字段变更仍走 `get_mut`（丢 claim、强制重序列化）。

### (d) 搜索 coverage + 续查推进

- 契约（`agent-contracts/src/context.rs`）：`ContextSearchCoverage { complete, unread_pages, stop, continuation }` + `ContextSearchCoverageStop { Complete|Budget|Deadline|HotCap|Unreadable }`（Display）；`ContextEngine` 新增两个带默认实现的方法：`last_search_coverage()`（默认 complete，不破坏任何既有实现）与 `search_external_continuation(query, token)`（默认忽略 token 转发 `search_external`，全内存引擎语义正确）。`EngineQuery::SearchExternal` 新增 `continuation: Option<String>`。
- 引擎：`search_with_continuation` 在 drain 前用 `rotate_search_window` 校验 token＋查询绑定（文本＋过滤器；limit 不参与绑定），把上次窗口换回 pending 队尾，返回**累积 covered 集**作为 drain 的 skip 边界——drain 用 `take_while(!skip)` 只读未走过的页，走完即 `HydrationOutcome::complete()`（coverage complete、不再发 token），遍历必然收敛而非反复轮转。`finish_search_hits` 每次记录 coverage；不完整时发新 token（窗口序号递增）；零命中＋不完整保持 B2 fail-closed，token 写进错误文本。已建 owner 的可寻址性全程不动。
- Core（`agent-core/src/kernel/mod.rs`）：continuation 非空路由到 `search_external_continuation`；非空不完整结果在**模型可见正文**追加 `[coverage] INCOMPLETE: N cold page(s) … continuation="…"`，与 result-capped 并列且相互独立（`result_capped` 与 `metadata.coverage` 分开上报）；零命中完整时行为不变。
- 工具（`tool-runtime/src/tools/context.rs`）：`context.manage search` 新增 `continuation` 参数与 schema 属性说明。

### (e) 命名收口

`hydrate_all_pending_cards` → `hydrate_pending_cards_within_budget`（5 处调用方同步）；不追加历史补丁式注释，函数文档只描述当前不变量（预算停止于批间与单次读取边界、队列永续可续）。测试模块文档中的旧名一并更新。

## 红→绿（验收反例，实际输出摘要）

新测试文件：`crates/context-simple/src/tests/fixed_budget_closure.rs`（7 例）＋ `agent-core/src/kernel/tests.rs::incomplete_search_coverage_reaches_the_model_body_with_a_continuation` ＋ `index/external.rs` 单元 4 例 ＋ `cold_bounds.rs` 尾段改为固定上限 continuation 走查（替代「提额排空全历史」旧形状）。

反例 1/2/3 的红在独立 `git worktree`（同 HEAD `4ba60b97`、仅加入兼容旧 API 的测试变体）中实际复现，随后删除：

- `a_single_slow_card_read_cannot_outlive_the_operation_deadline`：红 `RED (pre-fix): the drain must return on its own deadline, but it is still waiting on the unreleased pause: Elapsed`；绿：操作在预算内返回、`stopped == Deadline`、该行 pending 保持、放行后 drain 正常排空。
- `an_entry_larger_than_the_remaining_byte_room_stays_a_pending_owner`：红 `the hot map must not cross the byte cap: 1704 > 1065 (… stopped: HotCap, remaining: 2)`（旧形状装完才报、还吞了 2 行 owner）；绿：字节不越上限、4 行全保持 pending、大条目按 id fetch 取回正文且结算后仍在预算内。
- `sequential_fetches_over_a_fixed_hot_cap_page_the_whole_history`：红 `the hot directory stays within the fixed cap across the whole walk (after fetch 4: 5)`；绿：3C+2 条全部取回、每次 fetch 后热 ≤ C、热∪pending 与全历史集合相等且不相交。
- `incomplete_search_coverage_reaches_the_model_body_with_a_continuation`（agent-core）：先在契约/引擎/工具就绪、kernel 渲染未实现时跑出行为红 `the model-visible body must carry the coverage fact: context://run/…（普通命中行，无 coverage）`；实现渲染后绿：正文含 `coverage`、`40`、`cold-window-3`，`result_capped == false`（与 limit 截断区分），`metadata.coverage` 类型化，模型回传的 token 实际到达引擎。
- `search_coverage_names_the_gap_and_a_continuation_reaches_later_pages`：1 窗口命中＋10 未读页＋limit 20 → coverage `{incomplete, 10, hot-cap, token}`；每次续查推进未见页、热 ≤ 固定上限、走完 seen == 全集；新查询零命中在满员热表处 fail-closed 且错误带 continuation，走完自身区域后零命中才是 Ok（诚实稳态：pending 目录常驻可寻址行，不再承诺「排空后完整」）。
- `a_corrupt_page_does_not_block_the_continuation_walk`：坏卡如实计 missing、永不假称已读，13 个可读页全部经 continuation 可达，走完 complete。
- 反例 5（类型化背压）：`demote_overflow_reports_residual_backpressure_when_nothing_is_demotable`（无 claim＋pinned＋skip → `over_entries` 类型化残余）、`gc_reports_typed_hot_metadata_backpressure_when_nothing_can_demote`（gc 报告 `hot_metadata_backpressure == true`，owner 一个不丢）、`gc_settles_back_within_the_cap_when_demotable_entries_exist`、`demote_ids_rotates_exactly_the_listed_carded_entries`。此四例钉的是新增类型化面（红为编译级，无法在旧 API 上行为化表达，如实记录）。

## 验收命令与结果（Windows，Git Bash，全部实际执行）

```
cargo test -p context-simple   → 414 passed; 0 failed
cargo test -p agent-core       → 153+12+3 … 全部 0 failed（含新 kernel 测试）
cargo test -p agent-context-service → 11+7 … 0 failed
cargo test -p tool-runtime     → 278 passed; 1 ignored; 0 failed
cargo test -p agent-runtime    → 421/96/4/32/31/3/144 … 全部 0 failed
cargo test -p agent-contracts  → 183 … 0 failed（serde 兼容旧 checkpoint）
cargo test -p context-contextcore / context-baselines / agent-conformance / agent-compose / agent-eval → 0 failed（agent-eval 补 3 处 GcReport 字面量）
cargo clippy -p context-simple -p agent-core -p agent-contracts -p tool-runtime -p agent-runtime -p agent-eval --all-targets → 0 warning/error
cargo fmt -p <同上> -- --check → clean
```

注：`cargo test -p agent-core` 按完成任务要求执行（改了 kernel 输出协议）；`cargo test -p agent-context-service` 同（该 crate 依赖 contracts/context-simple，实际未改其源码）。

## 限制与未做

- **agent-context-service/context-contextcore 的 wire 层未传播 coverage**：进程边界 `ServiceOp::SearchExternal` 响应仍是裸条目数组（改响应形状需动 wire 协议与对端 parity fixture，超出本切片）。coverage 访问器在共享 trait 上（默认 complete），该层可在后续小切片补齐；进程外引擎（wire adapter）经 trait 默认实现表现为「coverage 完整」，不虚报。
- 续查状态（token→covered 集、coverage）为引擎内存态、不进 checkpoint：restore 后旧 token 失效、退化为全新搜索（旋转可逆，coverage 事实始终权威）——文档如实声明。
- 访问戳改为「非卡片相关」后，卡片重装/restore 可能带回较旧的访问时间戳（弱化 recency、更保守的老化方向）；卡片相关字段（semantic/residency/retention 等）变更仍丢 claim 并在下一次 capture 重序列化，语义终态不可能被旧卡片复活。
- pending locator 目录仍随历史增长（T4 已有事实，续审同样只要求类型化事实而非消灭它）；walk 完成后热表停在固定上限、pending 常驻可寻址行——「零命中完整」只在候选区域走完后对*该查询*成立。
- 反例 5 与 coverage 引擎级测试对旧 API 是编译级红（新增类型面本身），行为红只覆盖反例 1/2/3/4 四处，已在上面如实分列。
- 未改 agent-host、未运行任何 `git add/commit/push`；NEXT_TASKS.md 的 S3 关闭标注留给主会话（需要最终 SHA）。

## 主会话验收补充（2026-09-15，合入前）

- 合并验收在共享树（S3+S4 同时在飞）上重跑：`cargo test -p context-simple` 414/0、`-p agent-core` 153+12+3 全绿、clippy 七 crate `--all-targets` 0、fmt clean。
- **新增修复（本切片范围）**：CI run `34999486097`（windows part full）里 `externalize_growth_demotes…` 在 `captured_history` 断言 spilled 38/40 失败。根因：捕获的卡片写入有墙钟预算（`engine.rs` run_external_spill_io 的 `external_checkpoint_io_budget_ms`，默认 2s），满载 runner 上 40 次小文件写入越界后剩余条目按设计留在 inline、本条断言过窄。修复：`cold_bounds_config` 把该预算钉宽到 60s 并注明理由——墙钟噪声不是本模块钉住的对象；预算中止语义（超时条目留 inline、下次捕获续写）另有确定性覆盖。修复后 cold_bounds 2/2、全库 414/0。生产侧无缺陷：预算中止是文档化的性能旋钮，不是正确性缺口。
