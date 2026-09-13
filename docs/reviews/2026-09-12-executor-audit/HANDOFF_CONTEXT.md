# 交给上下文执行者

> 第一轮交接记录。当前开工改读 [第二轮 A 份任务](../2026-09-12-gc-core-followup/TASK_A_CONTEXT_GC.md) 与 NEXT_TASKS 顶部；不要重复已落地的 CTX-1。

执行 M18 的 A 部分。先读 `docs/CURRENT.md`、`docs/NEXT_TASKS.md` 顶部当前段，以及本目录 [REPORT.md](REPORT.md) 的 E01/E02/E04 和 [TASKS.md](TASKS.md) 的 A 部分。

**本次只交付 CTX-1：未知范围不能成为正文覆盖证明。** 核对工作树已有 CORE-1，不重做正确的窗口修复；先从最终 ModelRequest 构造 E01 反例，再修正渲染/定价/消费的一致依据，补必要回归和实施回执。CTX-2/3/4 是后续顺序，不在同一片打包实现。

拥有 context-simple、context-baselines、runtime/prompt.rs 的证据逻辑与正文缓存语义。共享 contracts 与 Actor/command/compose 由执行核心负责人合入；先给最小接口说明和 fixture。COST 线需要 prompt 改动时由你合入，不双写。

基线为 `685b6bbb` 加审查时未提交修改；先查 git 状态和当前实现。保留用户已有改动；不改 GC 权重，不换模型，不扩大缓存，不改任务完成权。不运行真实付费模型，不提交其他人的修改。定向验证完成即停止，回执交代默认路径、检查和限制。
