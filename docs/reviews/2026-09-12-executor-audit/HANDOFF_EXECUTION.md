# 交给执行核心负责人

> 第一轮交接记录。当前开工改读 [第二轮 B 份任务](../2026-09-12-gc-core-followup/TASK_B_EXECUTION_RECOVERY.md) 与 NEXT_TASKS 顶部；不要重复已落地的材料化 lane。

执行 M18 的 B 部分，并担任共享契约的单一合入负责人。先读 `docs/CURRENT.md`、`docs/NEXT_TASKS.md` 顶部当前段，以及本目录 [REPORT.md](REPORT.md) 的 E03/E06/E07/E08 和 [TASKS.md](TASKS.md) 的 B 部分。

**本次只交付 EXEC-1：材料化等待期间可取消。** 从真实 SimpleContextEngine＋可阻塞存储依赖复现 Actor 等待边界，再沿已有 operation lane 修复准备阶段的取消、join、rollback/fence 和晚到结果。维护已移出 Actor 不代表材料化也已移出；不要重复 W04 已完成部分。EXEC-2/3/4 为后续顺序。

拥有 RuntimeActor/Core 接缝、任务表、恢复、workspace/storage/host 及相关 SDK 消费。协调 A/C 提出的 contracts、command、compose、协议/DTO/fixture 增量；领域语义让提出方核对。单一合入不代表可擅自扩大对方功能范围。

先核对当前混合工作树并保留未提交修复；不得从裸 HEAD 覆盖它。Actor 是唯一编排者，Core 继续掌握 effect 权威；恢复记录不是重放许可。只跑相关回归，本片达到验收即停，回执诚实说明未完成的 CI/真实产品验证。
