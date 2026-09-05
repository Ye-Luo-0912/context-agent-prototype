# 当前工作

## 已核对的事实

审查代码基线：`12c86283b8d5991e9f17a07f14871dcf39d65066`。
这是源码快照，不是未来文档提交的 SHA，也不代表本地测试已通过。

- v0.1.0 已有 GitHub Release，名称为 alpha，带 Linux/Windows ZIP。
- M15 已有冻结 PASS 记录；LT-EVAL-06 已提交三类任务的记录。旧证据继续保存，不重新解释。
- 已有 TaskAnchor、ExecutionState、`task.manage`、检查点、恢复、审批、内建文件/搜索/编辑工具。
- 已有 `/status`、doctor、诊断导出、离线 Run Summary 和 shadow Frame 测量。
- `RuntimeHandle::continue_active_task()` 已存在，TUI 尚无 `/continue` 分支。
- Runtime 支持一条忙时输入排队，TUI 的 busy 分支却拒绝普通输入。
- deferred proof refresh 已有代码，TUI 仍显式设置 `defer_proof_refresh: false`。

远端 CI：该源码的 GitHub Actions run `33986702977` 已 completed/success，六个 job 均通过（Linux/Windows 检查与测试，以及文档检查）。这不是本地重跑结果，也不代表后续改动自动通过。

## 当前决定

优先做可用 Coding Agent，不再用“全部审计清零、重新评测、再建 Chronicle/TaskGraph”阻止功能开发。
默认只读本页与 [NEXT_TASKS.md](NEXT_TASKS.md) 当前工单；详细契约按改动模块查阅。

应用本包可完成 D0 的入口替换；下一项是 **F1：继续执行与输入反馈**。
执行者先完成 D0 最后的小范围一致性检查，不得停在文档整理或全仓测试上。

## 不在本轮

新 GC/排序算法、Frame-3 正式输入翻转、向量检索、通用 Planner、Chronicle 数据库、TaskGraph、并行 worker、插件平台和自动自修改。
已确认的当前路径错误仍需修复，但采用当前功能内的最小修复，不重开无限加固阶段。

唯一交付顺序：[ROADMAP.md](ROADMAP.md)。缺陷分流：[AUDIT_TODO.md](AUDIT_TODO.md)。
