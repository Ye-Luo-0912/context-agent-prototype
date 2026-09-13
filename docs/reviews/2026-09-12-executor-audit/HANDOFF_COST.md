# 交给缓存与成本执行者

> 第一轮交接记录。当前开工改读 [第二轮 C 份任务](../2026-09-12-gc-core-followup/TASK_C_COST_CACHE.md) 与 NEXT_TASKS 顶部；已落地的成本事件/输出 cap 不重做。

执行 M18 的 C 部分。先读 `docs/CURRENT.md`、`docs/NEXT_TASKS.md` 顶部当前段，以及本目录 [REPORT.md](REPORT.md) 的 E05、D02–D05 和 [TASKS.md](TASKS.md) 的 C 部分。

**本次只交付 COST-1：完整调用账本。** 先给 B 提交最小 usage/compaction 事件增量与兼容 fixture。修正普通失败/压缩失败不记账、压缩身份在事件投影丢失、下界/未知混入汇总的问题；保留部分已知用量。使用现有 journal/event/metrics，不新建数据库。COST-2/3/4/5 为后续切片。

拥有 provider、compactor、成本 metrics 和 GUI 成本消费面；Actor 发射点、公共 contracts/DTO/compose 交 B 合入；prompt.rs 交 A 合入。不得直接抢改共享文件。已有 CurrentStateLast、PromptReuseBoundary、ResponsesExplicit 与诊断工具不重做。

先核对当前未提交实现。默认 provider 行为保持；不换用户配置的主模型，不用本地前缀长度或 cache hit 比例声称省钱。本次用离线 fixture 完成账目功能，不读取凭据、不调用付费模型；后续真实对照必须固定环境和费用/请求上限。只跑相关验证，记录实际命令、结果、账目范围和限制，完成首片即停止。
