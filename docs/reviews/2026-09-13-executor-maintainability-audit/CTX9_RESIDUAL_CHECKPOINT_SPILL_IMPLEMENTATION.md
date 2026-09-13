# CTX-9 残余首片实施回执：checkpoint 外置尾分片（store 侧车卡片）

日期：2026-09-13。第三轮队列最后一行「CTX-9 / N7 元数据有界」的首个垂直片；A 域文件（context-simple），回执由本片执行者登记。

## 用户结果

长期运行下 checkpoint 不再随外置历史线性变大：超过内联目标的最旧 External 条目把元数据卡片写进既有 context store（内容寻址、幂等），checkpoint 只携带内联段＋`external_spilled` 寻址清单；restore 从卡片全量重水化，状态逐字节等价。搜索/召回/目录/材料化语义零改动——条目仍全部驻留内存，本片只压缩 checkpoint 字节（N7 的 checkpoint 面）；Runtime 内存分片（ExternalMap 溢出）沿同一卡片机制属下一片。

## 实现

- **store.rs**：`.card` 侧车（`external_card_path`/`external_card_bytes`/`parse_external_card`/`write_external_card_async`/`read_external_card_async`）——schema 信封＋id 校验；原子写（temp→sync→rename）；`.card` 后缀不匹配 reconcile 的 `.json` blob 扫描；`checksum_hex` 提为 crate 可见。
- **engine.rs capture**：超过 `external_checkpoint_inline_target`（默认 2048，opt-in 收紧）的最旧条目中，External 驻留、非 Pinned、非 keep_alive 者按外置序分片；卡片文件名＝`{id}.{内容哈希12}.card`——元数据未变跨 capture 命中同一文件（零重写、不耗预算），元数据变化产生新文件（旧文件成孤儿垃圾，有界：每次元数据变更 ≤1KB，启动 reconcile 对已删除 id 清理属下一片）；单次 capture 写入预算 `external_checkpoint_card_batch`（默认 64），耗尽时剩余条目本次保持内联（checkpoint 如实变大，跨 capture 收敛）；IO 失败同外部化失败语义——保持内联，宁可 checkpoint 大不丢恢复状态。checkpoint Value 注入 `external`（内联）＋`external_spilled`（`{id, hash}` 对象数组；为空时键不存在，既有 checkpoint 字节稳定）。
- **engine.rs restore**：从 Value 读分片清单；**先查重后重水化**——spilled id 已被内联 external 或 heap 持有＝结构性矛盾 fail-closed（敌意/损坏 checkpoint 不是旧格式）；op_gate 串行化下放锁做卡片 IO；缺失/损坏卡片＝该条目缺席＋`State.external_cards_missing` 如实计数，恢复整体不失败（已过保留窗旧 checkpoint 的诚实降级，与旧 checkpoint blob 缺失同型）；重水化条目按外置序并回 map、索引重建。
- **checkpoint.rs**：`spilled_entries_from_value` 解析；`recovery_item_ids` 并入 spilled ids——保留 checkpoint 的 Storage GC 保护根覆盖分片条目（blob 不被误删，CORE-3 保护链闭合）。

## 回归（`tests/external_spill.rs` 4 项，红检查在先）

1. `checkpoint_external_tail_is_spilled_and_restores_identically`——30 外置/目标 10：恰 20 条分片（最旧优先）、内联 10、卡片文件恰 20；第二次 capture 幂等（零新增卡片）；restore 后 30 条全在、驻留一致、`external_cards_missing==0`。
2. `a_mutated_spilled_entry_reflects_capture_time_metadata`——分片后变更条目访问戳再 capture：卡片携带 **capture 时点**元数据（内容寻址哈希随之变化），restore 反映 capture 时点状态。
3. `a_missing_card_degrades_the_restore_without_failing`——删一张卡片后 restore：整体成功、该条目诚实缺席、计数恰 1、其余 14 条全在。
4. `recovery_roots_cover_spilled_ids`——分片条目的 blob 全部仍在保护根集合。
- **红检查**：内联目标临时置 `usize::MAX`（＝分片前的 checkpoint 行为）→ 首测红于 `external_spilled` 键缺失；恢复后全绿。

## 验证

`cargo test -p context-simple --lib`：**376/376**（372＋4）；baselines **25/25**（引擎可替换性保持）；fmt/clippy 0 警告。整仓 `cargo test --workspace` 在本日相干窗口完成一次全绿（35 目标零失败，期间修复并行会话遗留的 agent-replay/agent-eval `TaskCompleted{artifacts,final_output_digest}` 4 处构造点机械适配与一处被改坏的 match 臂）。

## 第二片（同日续）：卡片生命周期闭合

- **卡片移入 `cards/` 子目录**（`external_card_path`/`external_cards_dir`）——与 blob 顶层 `.json` 命名空间彻底分离，reconcile 的 blob 扫描与清扫互不误触。
- **启动 reconcile 孤儿卡片清扫**（`run_reconcile_io_protecting` 尾段）：保护规则与 blob 完全镜像——id 在当前 external map（活跃条目，卡片服务未来 capture）或保护根（保留 checkpoint 的恢复承诺）**之外**即孤儿（条目已被 Storage GC 删除，或元数据更替后的旧哈希文件），删除并计入 `ReconcileIo.external_cards_removed` → 契约 `StoreReconcileReport.external_cards_removed`（serde default，旧报告字节稳定）；异形卡片名移入 `quarantine/`（与 blob 同型处置）；目录/读取 IO 错误如实计入 `io_errors` 与 reasons，不静默。
- 回归 `reconcile_cleans_orphan_cards_and_honors_protection`：30 外置/20 卡片 → 模拟 Storage GC 删除前 10 个分片条目 → reconcile 恰删 9 张孤儿卡、保护根覆盖的 1 张幸存、10 张活跃条目卡片保留（共 11）。
- 验证：context-simple **377/377**（372＋5）、fmt/clippy 0。

## 边界与下一片（如实记录）

- 本片收口 **checkpoint 面**；**Runtime 内存面**（ExternalMap/Catalog 溢出，条目离开内存）沿同一卡片机制属下一片——需要与搜索候选生成的完整性语义（Stored 候选）协同设计，未在本片偷工。
- ~~卡片孤儿垃圾~~：已由第二片收口——启动 reconcile 清扫（map∪保护根之外的卡片删除、异形卡 quarantine、计数上报）；运行期内新增孤儿（条目删除至下次重启之间）有界（≤1KB/条目/变更）。
- restore 对长历史的卡片读是 O(spilled) IO（每次读有界）；延迟换 checkpoint 字节的取舍已在代码注释与回执写明。
- 未提交/推送、未跑远端 CI；共享树并行域（A/B 第三轮收尾）在飞期间完成，全部验证在可编译窗口。
