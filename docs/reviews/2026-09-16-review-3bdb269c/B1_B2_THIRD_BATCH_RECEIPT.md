# B1/B2 回执 — 批量 required 有界解析计划与 existing card 认领校验

工单：`docs/NEXT_TASKS.md` 第五批 B1/B2 节（B3 已由 [`R6_R7_R8_B3_SECOND_BATCH_RECEIPT`](R6_R7_R8_B3_SECOND_BATCH_RECEIPT.md) 收口）。
执行日期：2026-09-17。基线：main `c5f5426a`（干净工作树）。B1 提交 `de6bf061`，
B2 提交 `2482f3ef`。本回执只记录本切片实际发生的事。

## 现场（实施前）

- **B1**：`resolve_required_cold_refs` 每个 exact ID 走 per-id lane 装卡，
  装完立即 `settle_metadata_residency` 且只 protect 刚装的 id；解析只保存
  `Installed/AlreadyOwned` 结果，不持已验证条目快照；`plan_required_with_resolution`
  在整批结束后才按热表查找。热上限之下，本批后装的卡把先装的挤回
  pending——规划找不到 owner，`Installed` 的兜底把 miss 压成 `Missing`
  （materializer.rs 的注释「The planning lookups find these」正是被驱逐
  打破的假设）。
- **B2**：`run_external_spill_io` 的 plan.writes 分支对已存在的卡片路径
  `try_exists` 后直接放进 `io.written/io.spilled`——不比对现有内容与计划
  字节，不走受检卡片读取。`checkpoint` 随后 `record_card` 并把条目排出
  inline 段：同名坏文件/目录占位被记成有效卡片引用，下次 restore 才发现。

## 机制（实施后）

### B1 — 解析时捕获版本/范围绑定的计划源

- 引擎侧：per-id lane 改返回 `PendingIdRead { outcome, entry }`——读成
  功（`Installed`/`AlreadyOwned`，含装卡与他人先装的并发分支）时携带卡片
  条目本身，即 pending 行授权的那个版本（身份、`blob_checksum`、owner
  元数据都绑定在该卡片哈希上），与后续任何一次重读该行所得一致。
- `RequiredColdResolution` 增 `resolved: Vec<(id, Box<ExternalizedContext>)>`：
  解析循环在读取点捕获（每 id 首捕获生效；上限
  `MAX_REQUIRED_PLAN_OBSERVATIONS`，超限丢弃——计划本就装不下更多必需
  正文，安装本身不受影响）。驻留结算不变：每次安装照样滑动热窗口，捕获
  只消除规划对「整批结束时谁恰好驻留」的依赖。
- 规划侧三处 fallback（都走既有 `plan_store_required`，经过同一
  policy/item-cap 门，`seen` 去重共用）：
  1. exact-id 分支：热表查不到 → 捕获条目规划（`blob_checksum` 供
     `realize_required` 校验读回正文）；
  2. 实体分支：驻留 `ids_for_entity` 之后 → 捕获条目按实体匹配补齐；
  3. 前景（`plan_foreground`）：`best_stored_file_body` 查不到 → 捕获条目
     按同谓词（`externally_retrievable`＋`is_file_body_entry`＋路径＋
     revision，同 `max_by_key`）规划——同病同修：批内后装的前景卡同样会
     把先装的前景卡挤出。
- 「真实预算不足」不改：`apply_required` 对放不下的必需正文本就报
  准确 `BudgetExcluded`；修复后它覆盖被驱逐目标（不再被 `Missing` 抢先）。

### B2 — 认领校验与诚实 inline

- `existing_matches`：先 `metadata` 守卫（是文件且长度与计划字节相等——
  长度不同不可能一致，不做整读），再整读比对计划字节。相等 → 认领
  （内容寻址幂等免重写，不变）。可读但不一致，或不可读（目录占位等）
  → 落入下方的 `write_external_card_async`（原子 temp+sync+rename）——
  即修复写入或首次写入同一条路径；写入失败（目录占位、权限、磁盘故障）
  保持内联，条目随 checkpoint 原样可恢复，manifest 不留坏引用。
- `recorded` 快路径（`card_hash` 命中即零序列化零 I/O）不动：稳态 capture
  对未变条目仍然免费。重认领路径（崩溃于写与记账之间、共享目录、异物）
  每行最多付一次长度守卫＋一次整读，每 capture 受既有 scan budget 约束。

## 回归（红-first，红相在被修改的 HEAD 上先证红）

新文件 `crates/context-simple/src/tests/batch_required_plan.rs`（B1，红于
`c5f5426a`）与 `crates/context-simple/src/tests/spill_claim_integrity.rs`
（B2，红于 `de6bf061`）：

1. `required_bodies_survive_demotion_caused_by_later_installs_in_the_same_resolution`
   （B1 核心反例）：hot cap=2＋required A/B/C 三个合法可降级冷页＋模型预算
   足够，restore 批次边界与 `external.get(A).is_none()` 证明 A 在规划时
   确实被 C 的安装挤回 pending。**红：A 报 Missing；绿：零 miss，A/B/C
   正文全部进帧。**
2. `required_over_demoted_cold_reads_report_budget_excluded_not_missing`：
   同 fixture、模型预算 1。**红：A 压成 Missing；绿：3×BudgetExcluded。**
3. `mixed_exact_entity_and_path_refs_survive_batch_demotion`：冷行
   [F1, A, B, C, F2]、cap=2，A（exact）被 B 挤出、B（实体）被 F2 挤出、
   F1（前景）被 C 挤出（逐条断言驱逐后的驻留状态，非 fixture 巧合）；
   前景条数恰为 `MAX_FOREGROUND_RESOURCES=2` 不触上限排除。
   **红：exact＋实体双双 miss；绿：A/B/C＋两条前景全部服务。**
4. `capture_repairs_a_same_name_card_that_does_not_match_the_planned_bytes`
   （B2）：capture 前在计划寻址路径预置同名坏字节（与
   `plan_external_spill` 同源序列化预测路径）。**红：坏字节被原样认领；
   绿：认领后路径上是计划字节，新引擎 restore 后按 id 读回原正文。**
5. `capture_keeps_an_entry_inline_when_its_card_path_is_a_directory`：
   卡片路径被目录占位。**红：manifest 留下指向目录的定位行、条目被排出
   inline、按 id 读取失败；绿：无定位行、元数据随 checkpoint 直接可恢复。**

红相输出（实测）：
- 1 → `panicked: every required body was read successfully this operation; nothing may miss … reason: Missing`
- 2 → `left: Missing  right: BudgetExcluded`
- 3 → `panicked: exact A and the entity ref (B/C) were all read this operation: […, "shared-required-entity"]`
- 4 → `left: [102,111,114,101,…] ("foreign garbage bytes…")  right: <planned card bytes>`
- 5 → `panicked: an unclaimable card path must keep the entry inline — no manifest row may point at it`

## 变异恢复复验（每处变异后运行三条/两条红线，再 sha256 外手工还原）

- B1 变异 A（删除 exact-id 捕获 fallback）→ **3/3 红**；变异 B（删除实体
  fallback）→ 混合反例红、其余绿；变异 C（删除前景 fallback）→ 混合反例
  红、其余绿。三处 fallback 各自承重。
- B2 变异（认领退回裸 `try_exists`）→ **2/2 红**。

## 验收命令与结果

- `cargo test -p context-simple` → **435 passed / 0 failed**（B1 后 433/0，
  B2 后 435/0；含全部既有回归）。
- `cargo clippy -p context-simple --all-targets` → 0 warning。
- `cargo fmt -p context-simple -- --check` → clean。
- 契约（agent-contracts）无变更；crate 外无新消费方。

改动文件：`context-simple/src/engine.rs`（`PendingIdRead`、
`hydrate_card_for_outcome`、`resolve_required_cold_refs`、
`run_external_spill_io`）；`context-simple/src/materializer.rs`
（`RequiredColdResolution::resolved/capture/resolved_entry`、规划三处
fallback）；新测试两文件＋`tests/mod.rs` 注册。

## 限制与取舍

- B1：per-id service 现在总是构造条目快照，`fetch_external`/`inspect`/
  directive 的 bool wrapper 丢弃它——每次按 id 服务多一次有界克隆
  （元数据＋≤`max_item_chars` 正文），在刚完成一次磁盘读＋反序列化的
  路径上可忽略，但确实存在。
- B1：捕获上限= `MAX_REQUIRED_PLAN_OBSERVATIONS`；超限后安装的条目无
  捕获（规划本就装不下，安装本身已按驻留规则结算）。已驻留条目仍由热表
  查找优先——捕获只是驱逐 fallback，不改变「热表元数据更新」语义。
- B1：实体 fallback 只匹配捕获条目的 `entities` 精确等值（与驻留
  `ids_for_entity` 同口径）；`MAX_FOREGROUND_RESOURCES`、
  `CONTEXT_CONSUMPTION_ACK_ITEM_CAP` 等既有上限行为不变。
- B2：文件权限故障未做专用反例——「修复写入」在 Windows（rename 覆盖
  只读文件失败 → inline）与 Unix（成功 → 修复）结果不同，但两侧都满足
  「要么修复认领、要么诚实 inline」的不变量；目录占位反例覆盖了写失败
  分支。
- B2：比对是整读等值，不是逐段流式比较——卡片字节受
  `external_checkpoint_card_bytes` 预算约束，且长度守卫先行，无界读入
  不会发生。
- 供应商净成本（缓存命中、真实端点接受）仍归 T8 条件实验，本切片未声称。
