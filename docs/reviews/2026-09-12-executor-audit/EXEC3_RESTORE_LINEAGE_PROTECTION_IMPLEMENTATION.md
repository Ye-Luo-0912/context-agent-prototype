# EXEC-3 实施回执：恢复后的活跃 artifact 引用受保护（M18 B 线第三片）

基线：`685b6bbb` ＋ 既有未提交工作树。对应 [REPORT.md](REPORT.md) **E07（P2）**、[TASKS.md](TASKS.md) EXEC-3。工作树落地（未提交、未推送、未跑远端 CI）。

## 用户能获得什么

一个长期任务反复冷恢复（33 次、64 次……）之后，其状态仍引用的第一代 sealed 快照/续读游标**保持可读**——保护依据是「恢复状态仍引用它」，不是谱系年龄。若承载容量确实不足，得到的是**类型化降级事件**（点名哪些前代不可读），模型/SDK 由此能区分「恢复任务成功」与「所需证据可继续读取」。

## E07 复盘与本片修复

**缺陷**：谱系链按年代淘汰——`admit_artifact_run_lineage` 只装「直接前代＋前代谱系」共 32 个，33 次恢复后第一代掉出窗口，其 sealed 引用永久拒绝读取；恢复后的谱系登记失败只发普通 Warning，「恢复成功」与「证据可读」不可区分。

**修复**：

- **workspace（保护式登记）**：`admit_artifact_run_lineage(current, predecessor, protected: &[RunId]) -> LineageAdmission { admitted, unadmitted }`。装入顺序＝直接前代 → **受保护集（恢复状态载荷中仍引用的前代）**→ 血统祖先按新近度填充剩余容量。受保护项**绝不因祖先年龄淘汰**；容量不足时溢出项进入 `unadmitted` 类型化返回——绝不静默按年代丢弃仍被引用的前代。读取授权语义不变：不在谱系内的前代依旧 fail closed，digest 校验照旧。
- **actor（引用提取）**：`collect_checkpoint_recovery_roots` 在遍历保留 checkpoint（恢复权威）时，同步从载荷字节提取 `artifact://run/<uuid>` 定位符的 run id（`extract_protected_run_ids`：仅认规范定位符，畸形 id 跳过不猜，去重上限 64）——保留的 checkpoint 本就是恢复根，其中仍被引用的前代即「活跃 sealed 引用」。启动 reconcile 调用方不需要该集（无 run 切换），忽略之。
- **类型化降级**：契约新增 `RuntimeEvent::RestoreEvidenceDegraded { unadmitted_runs }`。恢复提交后：部分装入 → 事件点名未装入前代；整个登记失败 → 事件列出全部受保护前代＋既有 Warning。恢复成功的定义不变，但「证据是否继续可读」第一次成为流上的类型化事实。事件为加法演进（.NET 事件体保留原始元素，未知 type 走既有 default 分支，无兼容性破坏）。

## 回归（agent-workspace 2 项＋提取器 1 项）

1. `a_still_referenced_first_generation_survives_deep_restore_chains`——**报告反例**：33 次恢复后纯血统链对第一代引用如实拒绝（旧边界保留）；随后保护式登记（受保护集＝[第一代]）后同一 sealed 引用恢复可读。「引用在，资格就在」。
2. `an_over_capacity_protected_set_degrades_typed_not_silent`——40 个受保护 run 超过 32 席（前代占 1）：装入 31、`unadmitted` 点名 9 个；已装入者经真实 sealed 工件验证可读。
3. `protected_run_extraction_reads_only_canonical_locators`——载荷含一个规范定位符＋一个畸形 id：只解析前者；无定位符的载荷保护集为空。
- 既有覆盖保持：链传递可读、损坏谱系 fail closed、未登记异 run 拒绝（含既有测试随新签名更新为空保护集）。

## 实际执行的检查（本机 Windows，2026-09-12）

- `cargo test -p agent-workspace --lib`：**110/110**（108＋新增 2）。
- `cargo test -p agent-runtime --lib`：**398/398**（含提取器单测）；`cargo test -p agent-runtime --test actor`：**86/86**。
- `cargo test -p agent-compose --test core3_restore_snapshot_paging`：通过（CORE-3 恢复分页走查在新登记签名下保持）。
- fmt/clippy（agent-workspace/agent-runtime）：本片干净（工作树仍有并行线在飞文件的瞬时状态，见下）。

## 边界与如实记录

- 谱系文件仍有界（32 席＋8 KiB）；受保护集以真实引用为界（一个任务实际引用的前代屈指可数），**超界必然类型化点名**，不按年代静默丢。
- 登记失败/降级不改变完成证明语义：同 task 完成验证仍走严格身份校验；谱系只授权**读取**，不构成副作用重放权或完成授权。
- **树上并行域如实记录**：验证期间 C 线 COST-1/2（ModelUsage cache-write 字段、压缩用量字段）与 A 线 CTX-2/3 在同批共享文件活跃落码，`agent-contracts`/`context-simple` 数次处于其编辑中间态；C 线落定后 agent-runtime 全绿。未提交、未推送、未跑远端 CI。
- **下一片**：EXEC-4（长状态载入与审阅在读入阶段有界）。
