# 当前工作

## 已核对的事实

审查代码基线：`12c86283b8d5991e9f17a07f14871dcf39d65066`。
这是源码快照，不是未来文档提交的 SHA，也不代表本地测试已通过。

- v0.1.0 已有 GitHub Release，名称为 alpha，带 Linux/Windows ZIP。
- M15 已有冻结 PASS 记录；LT-EVAL-06 已提交三类任务的记录。旧证据继续保存，不重新解释。
- 已有 TaskAnchor、ExecutionState、`task.manage`、检查点、恢复、审批、内建文件/搜索/编辑工具。
- 已有 `/status`、doctor、诊断导出、离线 Run Summary 和 shadow Frame 测量。
- `RuntimeHandle::continue_active_task()` 已存在，TUI 已接 `/continue`（2026-09-06）。
- Runtime 支持一条忙时输入排队，TUI 已把忙时普通输入交给该队列并按事件显示排队/应用/拒绝（2026-09-06）。
- deferred proof refresh 已有代码，TUI 仍显式设置 `defer_proof_refresh: false`（F3 接入）。

远端 CI：该源码的 GitHub Actions run `33986702977` 已 completed/success，六个 job 均通过（Linux/Windows 检查与测试，以及文档检查）。这不是本地重跑结果，也不代表后续改动自动通过。

## 当前决定

优先做可用 Coding Agent，不再用“全部审计清零、重新评测、再建 Chronicle/TaskGraph”阻止功能开发。
默认只读本页与 [NEXT_TASKS.md](NEXT_TASKS.md) 当前工单；详细契约按改动模块查阅。

应用本包可完成 D0 的入口替换；下一项是 **F1：继续执行与输入反馈**。
执行者先完成 D0 最后的小范围一致性检查，不得停在文档整理或全仓测试上。

D0 已完成（2026-09-06，`a28a5b9`）。F1 代码同日落地：TUI `/continue` 接
`RuntimeHandle::continue_active_task()`；忙时普通输入改交 Runtime 单槽队列，
排队/应用/拒绝按实际事件可见；命令错误走 notice 通道；同时修复
`context-simple` 的 release 消费盖章（`debug_assert!` 外移）与 materializer
PromptRequired 非 Pinned 条目重复选入。定向测试全绿（context-simple 281、
runtime actor 59 + turn 113、agent-tui 18、release consumption 8）。
手工走查（恢复一个未完成任务后 `/continue`、忙时排队处置）待做，完成后进入 F2。

F2 代码同日落地（`/work` 组合 set_focus + 空 requirement 集补 task.manage
PreferSurface + 一次 user-message；`/plan` 读新增的只读 `TaskPlanView`
查询；清单/TaskId 检查点往返保留有回归覆盖）。手工走查仍待做。

F3 代码同日落地：`--max-rounds=<N>` 以模型轮为单位严格解析并传入
ComposeConfig；预算耗尽（类型化 `Failure{RoundBudget}`）时 TUI 显示
`/continue` 续段、`/plan` 剩余清单与 `/checkpoint` 冷恢复前提；
`--defer-proof` 选择加入 deferred proof refresh，默认 inline 保持，
真实慢验证走查确认取消与清理后才改产品默认。deferred 取消/清理由既有
turn 套件（113）覆盖。下一步进入 F4（修改审阅与结果交付）。

F4 代码同日落地：`/review` 渲染事件派生的有界结果卡（变更文件取自工具
盖章的结构化路径、检查取自 verify.run/shell.exec/process.run 的真实结果、
失败与持久完成状态），任务完成时写入 artifacts/result-card-latest.json
供重启后查看；结果卡明确不归属用户原有修改，UI 不调用工具制造权威。
下一步进入 F5（多文件上下文与搜索实用化）。

F5 代码同日落地：文件体取代改为保守可证明规则（不同内容修订=过期边界；
同修订仅当新正文完整包含旧正文才取代，非重叠片段共存）；错误条目只被
verify.run 成功结果验证修复，读文件/grep 不再凭实体重叠清错；search.grep
在命中限额、文件预算或跳过文件时输出 PARTIAL 覆盖陈述与 scan_incomplete
元数据。GC/评分策略未动。下一步进入 F6（三类真实任务走查）。

## 不在本轮

新 GC/排序算法、Frame-3 正式输入翻转、向量检索、通用 Planner、Chronicle 数据库、TaskGraph、并行 worker、插件平台和自动自修改。
已确认的当前路径错误仍需修复，但采用当前功能内的最小修复，不重开无限加固阶段。

唯一交付顺序：[ROADMAP.md](ROADMAP.md)。缺陷分流：[AUDIT_TODO.md](AUDIT_TODO.md)。
