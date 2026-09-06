# 当前工作

## 现在做什么

**当前工单：无**（深入续审的可执行项与 M16-02 完成语义展示均已关闭；剩余为 PROCESS-01 Unix 持久监督身份设计与 M16-08 发布收尾，均为条件项）。

先按审查剩余队列逐项修代码里还在的缺口，再回到 M16-02 的完成语义展示。
这不是新的前置阶段，也不是全仓审查完成。一次一项；STORAGE-01 与 DOC-01 已关闭。

原文与探针：[reviews/2026-09-06-deep-audit/REVIEW.md](reviews/2026-09-06-deep-audit/REVIEW.md)。
可执行步骤：[NEXT_TASKS.md](NEXT_TASKS.md)。

## 审查剩余（排在 M16 产品剩余之前）

基线仍是 `12c86283b8d5991e9f17a07f14871dcf39d65066`（审查固定 SHA，不是本工作树 HEAD）。
本工作树 HEAD：`2e825d0`。工作区另有未提交改动（N2、STORAGE-01、审查入库、M16 文档）；不得 reset/stash。

| 顺序 | 工单 | 用户/系统得到什么 | 状态 |
|---|---|---|---|
| 1 | STORAGE-02 | 压缩元数据已发布后若后续失败，不能继续往旧代写 | 已关闭（2026-09-06） |
| 2 | PROCESS-01 | 宿主验证硬崩溃后不留下无监督子进程 | Windows 围栏已落地；Unix pre_exec 静默不执行（探针实证），改走持久监督身份设计 |
| 3 | PROCESS-02 | reap 未确认退出不清 pid | 开放 |
| 4 | WORKSPACE-01 | 普通 confined open 不阻塞 FIFO | 开放 |
| 5 | WORKSPACE-02 | Windows 拒绝路径立即接管 HANDLE | 开放 |
| 6 | PROVIDER-01 | 非 2xx 错误 body 有界读取 | 已关闭（2026-09-06） |
| 7 | PROVIDER-02 | Chat `length` 终止不丢成正常完成 | 已关闭（2026-09-06） |
| 8 | PROVIDER-03 | Responses EOF 尾帧与正常帧同一套校验 | 已关闭（2026-09-06） |
| 9 | CONTEXT-01 | 依赖候选按 newest-first，不先截旧前缀 | 已关闭（2026-09-06） |
| 10 | PACKAGE-01 | 构建输出与打包复制源绑定 | 下次实际发布时 |
| 11 | MCP-01 | 写/连接/读都可取消 | 仅默认产品启用 MCP 时 |

本分支已关闭、不重复立项：STORAGE-01、DOC-01；上轮 EOF wait、消费 ACK、PromptRequired 判重、resync / run_summary。
Windows 在 metadata 替换中途杀进程仍未注入，记为 STORAGE-01 测试缺口，不是新工单。

## 已核对的事实

- v0.1.0 alpha 已发布；M15 / LT-EVAL-06 证据保留，不重开、不改写。
- 任务/计划/继续/恢复的基础已在 Runtime。默认 `OperatorClosureOnly`：普通 final 结束执行段，不是模型自行持久关闭任务。
- `--max-rounds` 计量模型决策轮（`turn.model_round`）。
- 远端 CI run `33986702977` 在基线 SHA 上六个 job success；不是本工作树、也不是全仓审查完成。

## M16 仍在，但排在审查剩余之后

活动大阶段仍是 **M16：可持续交付的本地单 Agent**。不建 Chronicle、TaskGraph、通用调度或第二套编排器。
M16-00 文档已切换。M16-01/05/07 与大部分 02/03/06 已落地。审查剩余与 M16-02 完成语义展示均已关闭（2026-09-07）。下一产品项是 M16-08 发布收尾（PACKAGE-01 绑下次实际发布 + TUI 交互走查）。

对照表与限制见 [ROADMAP.md](ROADMAP.md)。缺陷细节：[AUDIT_TODO.md](AUDIT_TODO.md)。
无头 live：[walkthroughs/2026-09-06-f6.md](walkthroughs/2026-09-06-f6.md)。
提案原文不进默认必读：[reviews/2026-09-06-m16-proposal/TRIAGE.md](reviews/2026-09-06-m16-proposal/TRIAGE.md)。

## 不在本轮

Chronicle 数据库、TaskGraph、并行 worker、向量检索、Frame-3 正式翻转、SIEVE/TinyLFU 调参、新评测总门禁。
不能用“再跑一些测试”去换一个从未授予的自动完成权。
