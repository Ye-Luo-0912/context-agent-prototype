# 产品功能路线

> 状态：M16 产品闭环。**当前执行插入：** 2026-09-06 深入续审仅剩 PROCESS-01 Unix（管道 EOF 看门狗已落地 `a954314`，等 Linux CI 实证）；M16-02 已于 2026-09-07 关闭；原手工走查已转为自动化 E2E。
> 旧报告中的“先全部可靠性收口、再 Chronicle/TaskGraph、再开发功能”不是当前执行顺序。
> 不改变 Core 安全边界，不改写历史实验结果，不取消现有 CI。

## 目标与边界

用户给出一个仓库级任务，Agent 能维护短计划、查读修改、响应补充、有限执行、停止后续跑、冷恢复，并交付可审阅结果；同一套能力可通过非交互入口被脚本调用。

默认范围：单用户、单工作区、一个活动任务焦点、一个 RuntimeActor、动态进程内 Context。
不是通用 Agent OS，也不是把任意自然语言需求自动证明正确。

已经发布的 v0.1.0、已有 M15/LT-EVAL 记录、doctor、检查点是基础，不再作为待重做事项。
**实现存在、测试通过、默认启用、真实任务跑通**分别记录。

## 当前插入：深入续审剩余

按 NEXT_TASKS 前列处理仍开放的审查缺口：当前 **PROCESS-01**（Windows 围栏已落地；Unix 待持久监督身份设计）；PACKAGE-01 留到下次实际发布；MCP-01 仅默认启用 MCP 时。

STORAGE-01/02、PROCESS-02、WORKSPACE-01/02、PROVIDER-01/02/03、CONTEXT-01 与 DOC-01 已关闭（2026-09-06）。不重开 M15，不把 13 项清零当成新阶段，不新建总门禁。

<a id="route-to-a-usable-local-agent"></a>

## M16 顺序

M16 替换此前 D0/F1–F6 活动队列。审查剩余是同一队列的前列，不是第二套待办。可执行步骤见 [NEXT_TASKS.md](NEXT_TASKS.md)。

| 切片 | 用户可见结果 | 本分支 |
|---|---|---|
| M16-00 活动路线 | 开发者只看到一条当前队列 | 本文档切换 |
| M16-01 继续与补充 | `/continue`；忙时输入可见处置 | 已关闭（2026-09-07）：走查转为 TUI 会话循环 E2E |
| M16-02 工作模式与完成语义 | `/work` `/plan`；待审阅 ≠ 已持久完成 | **已关闭（2026-09-07）**：待审阅/持久完成显式区分 |
| M16-03 有限执行与取消 | 模型轮预算让出后显式续跑；慢验证可取消 | 主体已落地；Provider 输出边界已关闭；Unix 硬崩溃监督见 PROCESS-01 |
| M16-04 冷恢复 | 验证过的检查点恢复同一任务并继续 | 已关闭（2026-09-07）：产品配置冷恢复走查落地 |
| M16-05 结果审阅 | `/review` 区分已知改动、真实检查、未验证 | 已关闭（2026-09-07）：双工作区走查转为无头 + TUI E2E |
| M16-06 上下文与搜索 | 互补片段不互冒覆盖；搜索不完整如实说明 | 主体已落地；CONTEXT-01 与 FIFO/句柄项已关闭 |
| M16-07 非交互入口 | 同一 Compose/RuntimeHandle 的 JSONL 调用 | N1/N2 已落地 |
| M16-08 试用收口 | 对应源码的包 + 三类真实任务记录 | 无头 live 已有；打包来源与 TUI 走查待做 |

M16-00–07 全部关闭（2026-09-07），走查均为自动化 E2E；PROCESS-01 仅剩 Linux CI 实证。剩余条件项：M16-08 真实 provider live 记录、PACKAGE-01 绑下次实际发布、MCP-01 绑默认启用。08 收口整个阶段，不阻止前面切片独立试用。

进程内 `/continue` 可以先用；可靠冷恢复、可取消宿主验证、发布包正确性，须在各自问题处置后才能宣称完成。

## 测试如何配合主线

开发循环用定向测试和短演示；集成复用现有跨平台 CI。相同源码、相同命令，没有新原因不反复重跑。
三个真实任务确认产品闭环，不声称普遍成功率。真实 provider 不可用写 `NOT_RUN`，不假绿，也不因此倒退去建基础设施。

## 核心资产：本阶段必做 vs 暂不扩展

- Context：资源版本与证据片段分开；最终渲染与消费 ACK 一致。不重写完整 Frame 编译器。
- GC：attention / semantic / residency 分离；语义终结有可信依据。不用缓存热度裁决真假。
- 搜索：精确定位、正确候选顺序、分页与扫描完整性分开。不上向量库。
- 调度：单 Actor；修真实阻塞的取消与清理。不新增通用 Scheduler。

算法候选（BM25、边际收益装配、SIEVE/TinyLFU）只在真实使用暴露对应瓶颈后单独立项，一次只改一个主机制。M16 不以新算法胜出为前置。

<a id="post-m15-candidate-order"></a>
<a id="post-m15-candidate-order-proposal"></a>

## 不进入本阶段

Chronicle 数据库、结构化 TaskGraph、多 Agent、worker、MCP/插件生态扩展、递归自修改。
需要这些时先指出现有简单路径解决不了的具体用户任务。

## 参考文档

[CURRENT.md](CURRENT.md) 记录事实；[AUDIT_TODO.md](AUDIT_TODO.md) 分流缺陷。
架构、执行、恢复和权限修改分别参考 [ARCHITECTURE.md](ARCHITECTURE.md)、[EXECUTION_MODEL.md](EXECUTION_MODEL.md)、[RECOVERY_RUNBOOK.md](RECOVERY_RUNBOOK.md)、[PLATFORM_SECURITY.md](PLATFORM_SECURITY.md)。
提案原文：[reviews/2026-09-06-m16-proposal/](reviews/2026-09-06-m16-proposal/TRIAGE.md)。
