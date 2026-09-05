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

F6 记 NOT_RUN（2026-09-06）：三类真实任务（修 bug、加小功能、跨文件重构
并中断续跑）必须由操作者在真实 provider 与实际 TUI 的交互会话中执行，
本执行环境无法替代，也不假装 live 通过。待操作者完成的走查清单：
恢复未完成任务后 /continue；忙时排队/拒绝处置；/work + /plan + 清单随
恢复保留；--max-rounds 预算停止与 /continue 续段；--defer-proof 真实慢
验证（确认取消与子进程清理后才改产品默认）；/review 双工作区（干净 +
含用户旧修改）区分归属；跨文件任务的片段共存与搜索覆盖陈述。

2026-09-06 补充（无头验收 + 两处接线修复）：新增
`agent-compose/tests/route_flow.rs`，用真实组合根 + 脚本化模型端到端验证
/work 组合、task.manage 计划、预算停止与 /continue（含跨检查点恢复）。
途中修复两个真实缺陷：(1) 组合根未把 artifact store 接到 Runtime 服务的
检查点存储，actor 自动安全点在产品组合中写不出去；(2) 预算中止路径不落
安全点，写入在飞时留下的债务永远无人捕获，/continue 被 fence。两处均已
修复并由该测试判别覆盖。

2026-09-06 续审合并（同一源码基线 `12c8628` 的第二轮审查，报告与证据在
[`reviews/2026-09-06-continued-audit/`](reviews/2026-09-06-continued-audit/REVIEW.md)）。
四项新发现已在本分支逐条核实仍存在，归入既有工单，不新增前置阶段：

1. **F3-6a**：`tool-runtime` 的 process.rs/shell.rs 在输出 EOF 且进程未退出时
   于分支内直接 `child.wait().await`，绕过原 timeout/cancel 选择——子进程
   关闭输出后仍运行即触发（OS 探针已验证触发前提）。
2. **F3-6b**：`HostProofVerifier` 给 runner 新建独立取消 token，未接 Actor
   取消信号；需要“取消 → 现有进程清理 → 回收 → 可见确认 → 迟到结果拒绝”。
3. **F4 同批**：`AppState::resync_projection` 按 mtime 升序取 16 实为最旧文件、
   不按当前 run 过滤、无重放水位、整文件读入后才检查 32 MiB；
   `agent-replay` 的 run_summary 读取不存在的 `required_misses.total` 字段，
   必需上下文缺失在离线汇总漏计（Runtime 完成门禁不受影响）。
4. **观测口径（研究轨，随 F4）**：shadow frame 把 zone 上限截断与重复省略
   混计入 `duplicates_removed`，且非完整请求镜像；只修观测，不翻正式 prompt。

续审复核的两处旧问题（消费盖章在 `debug_assert!` 内、PromptRequired 判重）
在 `c6fbbab` 已修复。另更正一处过报：F5 的 PARTIAL 覆盖标注目前只覆盖
search.grep，fs.list 未做。研究轨候选（片段身份/曝光账目、短工作焦点、
装配边际收益、遮罩基线、SIEVE/TinyLFU 定位、词法特征贯通最终排序）已并入
NEXT_TASKS，均不阻塞 F1–F4。

## 不在本轮

新 GC/排序算法、Frame-3 正式输入翻转、向量检索、通用 Planner、Chronicle 数据库、TaskGraph、并行 worker、插件平台和自动自修改。
已确认的当前路径错误仍需修复，但采用当前功能内的最小修复，不重开无限加固阶段。

唯一交付顺序：[ROADMAP.md](ROADMAP.md)。缺陷分流：[AUDIT_TODO.md](AUDIT_TODO.md)。
