# W1 回执 — 逻辑 owner 与冷页解析（V1＋V2）

工单：`docs/NEXT_TASKS.md` 第四批 W1 节；缺陷细节：本目录 `REVIEW.md` 的 V1/V2 节。
执行日期：2026-09-14。基线：工作树（HEAD `b6ffc514` 之上的未提交变更；同树有
并行 W2 切片的未提交变更，见「限制」）。本回执只记录本切片实际发生的事。

## 现场（实施前）

- `engine.rs::gc` 执行预算化 hydration 后丢弃其结果（`let _hydration = …`），
  `plan_full_gc` 的两个退休调用点（正常路径与 body-free 空路径）都拿不到
  闭包完整性。
- `scope.rs::retire_closed_scopes` 的 referenced 集合只枚举 heap /
  `pending_externalize_retry` / `eviction_buffer` / 已加载 external /
  active scope——未加载 pending 卡片（`(id, card hash)` 定位行）中的
  `scope_id` 不可见。
- `engine.rs::hydrate_card_for` 读到卡片但 `scope_id` 不在 scope tree 时删除
  pending 定位行、计入 missing、返回 false（N03 结构校验，本切片保持不动）。
- `materialize` 直接 `plan_required`：精确 ID/URI 只查
  heap/retry/warm/已加载 external 索引；实体/路径查询同样只用已加载索引；
  不命中一律 `Missing`。

## 机制（实施后）

### V1 — scope 退休许可（保守推迟＋有界磁盘侧引用探测）

- `scope.rs` 新增 `ScopeRetirementPermit { pending_scope_refs, pending_unknown }`：
  - `retire_closed_scopes(state, target, permit)`：`pending_unknown` 为真时本轮
    **不退休任何节点**（保守推迟；RetiredScopeNote 环不因此溢出——推迟不产生
    note）；否则 referenced 集合并入 `pending_scope_refs` 后按原判定执行。
    既有 completed_task_facts/TaskAnchor/bottom-up 链完整性语义未动，只增加
    引用来源。
- `engine.rs::gc` 不再忽略 hydration 结果：闭包完整（pending 队列排空）→
  `closure_complete()`（无额外 IO）；不完整 → `probe_pending_scope_references()`
  对剩余 pending 行做**纯读**探测：逐卡读取（`read_card_with_test_hooks`，
  与既有测试钩子一致），可读卡贡献其 `scope_id`；缺失/损坏卡视为无引用
  （死行，drain 的 missing 记账仍独占其消费）；预算（`HydrationBudget`
  同口径：items＋绝对 deadline；热上限不适用——不安装）耗尽或瞬时 IO 失败
  → 剩余 unknown → 全部推迟。
- 收敛性：GC 每轮 drain 消费 `external_hydrate_max_items` 行（预算受限场景
  队列单调缩短），排空后退休恢复；热上限钉死、drain 停在 HotCap 的场景由
  探测兜底——探测条数预算 ≥ 队列长度即可证明闭包、正常退休继续。回归
  `gc_retirement_resumes_once_the_pending_queue_drains` 钉住「首轮推迟→
  多轮 GC 后收敛退休」。
- `ContextGcReport` 新增 `scope_retirement_deferred: bool`（serde default，
  旧报告反序列化兼容）；`plan_full_gc` 空路径同样上报推迟而非静默 None。

### V2 — 必需正文规划前的有界冷页目标解析

- `engine.rs::hydrate_card_for` 重构为 `hydrate_card_for_outcome`（类型化
  `PendingIdOutcome { Installed, AlreadyOwned, Missing, Corrupt, IoFailed,
  NoPendingRow }`）；原 bool 包装保留给 fetch/inspect/directive 调用点
  （只有 Installed 为 true——行为不变）。坏卡仍按既有口径计入
  `external_cards_missing`，但类型化结果能区分「读取失败」与「不存在」。
- `engine.rs::materialize` 在 plan 前调用 `resolve_required_cold_refs(&query)`
  （op_gate 内、state 锁外做盘 IO，与既有 materialize 形状一致）：
  - 精确 ID/`context://run/<id>` URI（`parse_ref` 两者都收）→ 逐声明走
    **per-id 服务 lane**（一次一卡、不受预算/热上限阻断、自带驻留结算），
    结果进 `per_id` 表——复用 `fetch_external` 的既有 lane，没有第二套。
  - 实体/路径引用（解析不成 id 的 claim）＋当前 foreground 路径 → 对
    pending 行做**有界扫描**（同一 `HydrationBudget` 口径：items＋deadline）；
    命中键的卡经 per-id lane 安装；只有「已决」的读（可读卡或已证死行）计入
    examined——瞬时 IO 失败/超时的行保持未读计数，缺席结论不可证明。
  - 无声明或 pending 为空时零 IO。
- `materializer.rs` 新增 `RequiredColdResolution { per_id, pending_unread }` 与
  `plan_required_with_resolution`（原 `plan_required` 保留为测试用的默认解析
  包装）；claim 未匹配时：
  - 精确 id → `per_id` 的类型化原因（Corrupt→Corrupt，IoFailed→IoFailed，
    Missing/NoPendingRow→Missing）；
  - 实体/路径且 `pending_unread > 0` → 新增的
    `ContextMaterializationMissReason::UnreadColdPage`（未证明缺席）；
  - 实体/路径且冷目录已读尽 → Missing（证明过的零命中）。
- contracts 变更（serde 兼容）：`ContextMaterializationMissReason` 新增
  `UnreadColdPage` 变体（snake_case 序列化，旧值不变）；`ContextGcReport`
  新增 `scope_retirement_deferred`（`#[serde(default)]`）。全仓无该枚举的
  穷举 match；`render_required_misses` 用 `{:?}` 自动渲染新原因。

## 回归（红-first，全部先在 HEAD 上证明红）

新文件 `crates/context-simple/src/tests/cold_owner_and_required.rs`（共享
fixture：全部冷条目卡片化＋restore 只装首批＋固定 bulk 水化条数，目标卡片
的 pending 状态逐条断言，不靠 fixture 顺序巧合）：

1. `gc_retirement_keeps_a_scope_referenced_by_an_unread_pending_card`
   （V1 反例）：卡片 A 引用已关闭 tool scope S（S 在
   checkpoint 捕获后关闭，close 只重标记已加载条目，未加载卡片合法地继续
   引用 S）→ restore 后 A 在 pending 尾（restore 批次边界断言）→ GC
   （bulk 水化条数 0，A 可证未加载）→ **红：fetch_external(A) 返回
   None（定位行被消费）**；绿：fetch 返回正文、`entry.scope_id == Some(S)`、
   S 仍在树中、owner 恰好一个；checkpoint/restore 后仍可读。
2. `gc_retirement_resumes_once_the_pending_queue_drains`（V1 收敛）：7 卡、
   restore 装 2、每轮水化/探测 2 条。**红：首轮 GC 立即退休（无保守推迟
   ——`T 仍在树中` 断言失败）**；绿：首轮 `scopes_retired == 0`＋
   `scope_retirement_deferred == true`，至多 8 轮内 T 完成退休。
3. `a_prompt_required_ref_to_a_pending_card_is_resolved_and_served`（V2
   反例）：目标在 restore 首批之后＋固定热预算＋精确
   PromptRequired `context://run/<id>` URI 直接 materialize。**红：
   required_misses 报 Missing**；绿：无 Missing、正文进帧。
4. `required_miss_reasons_distinguish_absent_corrupt_and_unread`（V2 原因
   区分）：真不存在 id → Missing（HEAD 上即绿，pin）；坏卡（覆盖卡片文件
   为垃圾字节）→ **红：Missing；绿：Corrupt**；实体引用＋pending 未读 →
   **红：Missing；绿：UnreadColdPage**；对照引擎排空队列后同一实体引用 →
   Missing（证明过的零命中）。

红相输出（HEAD，2026-09-14 实测，4/4 红）：
- 1 → `panicked: an unread pending card referencing a closed scope must still fetch by id`
- 2 → `panicked: while the pending cold directory is unproven, retirement defers`
- 3 → `panicked: … must not be reported Missing: … reason: Missing`
- 4 → `assertion left == right failed … left: Some(Missing) right: Some(Corrupt)`

## 验收命令与结果

- `cargo test -p context-simple` → **425 passed / 0 failed**（含并行 W2 切片
  的测试与全部既有测试）。
- `cargo test -p agent-core` → 153 + 12 + 3 passed / 0 failed（contracts 变更
  后）。
- `cargo test -p agent-runtime --test turn` → 146 passed / 0 failed
  （required_misses 语义变化后）。
- `cargo clippy -p context-simple --all-targets` → 0 warning。
- `cargo fmt -p context-simple -- --check` → clean。
- 交叉编译检查（枚举消费方）：`cargo check -p agent-eval -p
  context-baselines -p agent-replay -p agent-context-service` → 无错。

改动文件：`agent-contracts/src/context.rs`（＋变体＋字段）；
`context-simple/src/{engine.rs, scope.rs, materializer.rs, gc/full/mod.rs}`；
`context-simple/src/tests/{cold_owner_and_required.rs（新）, mod.rs,
gc_bounds.rs（plan_full_gc 签名跟随）}`。

## 限制与取舍

- **热上限钉死＋pending 队列长于探测条数预算**（默认 4096）时退休推迟会
  持续：这是「不能证明就该推迟」的诚实保守行为，报告有
  `scope_retirement_deferred` 可观测；长期解是随分页 owner/迁移/checkpoint
  原子维护的 scope 引用计数索引（本轮未做，未建第二份真相）。
- V2 的实体/路径解析是**一次性有界扫描**，不是常驻索引：同一 materialize
  里未命中键的行下次操作重读。pinned 正文位于 pending 卡片时对 pinned
  规划仍不可见（pinned 集合只来自已加载索引；不产生错误 miss，只是不主动
  解析）——沿 review 的最小修复范围（PromptRequired 精确引用）。
- foreground 的 `optional_misses` 未做 UnreadColdPage 区分（扫描会安装命中
  的 pending 文件体，使正文可提供；但未命中仍报 Missing）——review 的
  「按路径 foreground」补充测试未纳入本轮四条红线。
- 服务模式（ContextServiceAdapter）的 V2 等价性未测——属 W2/V3 的 service
  wire 范围。
- 本切片与并行 W2 切片同树开发：W2 在 `engine.rs` 加了
  `search_continuation_probe`、新增 `tests/search_continuation.rs`；本切片
  曾对 `cargo fmt -p context-simple`（crate 级，含其未格式化代码）与一处
  其引入的 clippy collapsible-if（`search_with_continuation` 内）做了机械
  修正以满足本 crate 的 fmt/clippy 门。两切片的语义改动互不重叠。
- Windows 下出现多次 test exe 僵尸进程占用链接输出，`taskkill` 清理后重跑
  即绿（工单预告的已知现象）。
