# 非客户端修复与核心设计复核

2026-09-07。基线仍为 `92f8d92af93f7478ca1a5c1de11c38519e46c6a5` 加工作区已有的 M17 修改。本轮按用户要求修复非客户端问题，并继续检查 Core、Context、GC、搜索、工具执行和恢复。修改保留在工作区，没有提交、发布或修改冻结实验数据。原始 `REVIEW.md` 和探针证据保留原貌。

## 本轮修复

| 原报告 | 修复结果 |
|---|---|
| R01：只读 Git 启动外部程序 | 从绝对宿主 PATH 解析 Git；用临时私有 Git 配置读取原对象库和索引。只保留换行符、文件模式等标量设置，禁用外部 diff、textconv、clean/process 过滤器、fsmonitor、分页器及子模块递归。原索引和配置不修改，兼容 staged 和 split index。 |
| R02：大 diff 管道阻塞 | 同时读取 stdout、stderr 并等待子进程；发现、执行、管道 EOF 和产物写入共享截止时间。每条管道最多保留 4 MiB，产物最多 8 MiB，达到上限后继续排空并明确报告截断；取消后的清理等待有界。 |
| R04：订阅遗漏事件 | 在 Actor 快照屏障前注册接收器；当前没有重放能力，任何不等于当前水位的重连游标均要求重新取快照。状态读取失败返回错误，拒绝订阅时返回已关闭的接收器。审批响应超时不再伪装为“已无待审批”。 |
| R05：verify.run 漏接 watchdog | VerificationRunTool 构造函数必须接收宿主监督配置，registry 将与 process.run 相同的配置传入，防止内部重新落回 false。 |
| R06：测试目标无法编译 | 补齐契约夹具字段、协议类型、导入、共享对象所有权，以及 continue_active_task 返回 TaskId 后的测试匹配。Rust 全工作区 all-targets 检查通过。 |
| R10：忙碌时重复提交被误拒绝 | 先验证请求、查询既有回执，再对新提交检查 idle；执行中的相同请求返回 AlreadyAccepted，不重复执行。 |
| R11：验证错误关联过宽 | 新增宿主类型化 VerificationProbe，绑定配方 ID、修订及完整定义/覆盖声明摘要；同时匹配任务 ID。关联随驻留层和 checkpoint 保存，成功观察也保留证据身份。恢复后的待处理验证队列再次核验双方关联，旧的仅名称关联不能升级为证明。 |
| R12：恢复接受 scope 环 | 检查 scope ID 唯一性、父引用、活动引用及无环性；拒绝恢复不替换原状态。关闭子树改为有访问集合的遍历，祖先查询增加步数上限。 |
| R14：工具正文伪造审批状态 | ApprovalDenied 仅由 Core 的审批分支类型化写入；删除自由文本分类器中的审批推断，加入外部正文伪造与真实审批拒绝的回归测试。 |

按要求跳过客户端 R03、R07、R08、R09、R13，没有修改 .NET/Avalonia 客户端实现。

扩查时另外修复了以下问题：

- **Storage GC 证据链误删**：旧代码仅让 StorageRequired 指定记录免删，没有把它作为强引用遍历的根。新增用例复现了“应删除 1 份，实际删除 3 份”。修复后保留锚点及其传递证据，仅删除弱关联记录；语义终态仍不复活。使用工作队列遍历强引用，避免反向长链反复扫描整个外部表。
- **搜索与导航结果的内存上限**：目录遍历增加 50,000 个目录项和累计 8 MiB 路径预算，空目录不能绕过文件数限制。grep 命中行截取匹配附近的 1,024 字节片段；符号名限制为 256 字符。覆盖不完整与片段裁剪分别报告。诊断展开按实际读取字节执行上限，不只依赖读取前的文件大小。
- **错误复现关联**：同文件实体相交不再足以使旧错误终结；只合并同任务、同来源、同验证定义且正文一致的保留记录。外部摘要不能证明正文相同，保守保留到验证或明确关闭。
- **监督台账残余**：读取本身限制为 64 KiB/256 行；线程和稳定伴随文件锁串行化读改写；新增记录也使用同步临时文件加原子替换，移除“替换失败先删旧文件”的分支。Unix 同步父目录，watchdog 强杀后的第二段等待也有截止时间。

## 设计判断

**保留 RuntimeActor 与 Core 的分工。** RuntimeActor 组织任务、轮次和工具流程，Core 决定审批、权限、effect 身份、提交及恢复。平台提供类型化操作和状态投影。Context 不应决定副作用是否提交，工具正文不应产生权限，客户端也不应从文字猜测完成。这次审批修复就是将状态的产生放回有证据的分支，而不是增加一个更复杂的文字分类器。

**上下文应是有证据来源的工作集，不是任务真相的第二份实现。** attention、semantic、residency 三个维度继续分开：热度回答“下轮是否值得展示”，语义状态回答“这条记录是否仍有效”，驻留状态回答“正文在哪里”。摘要、实体相似、检索命中都可以改变展示优先级，不能单独证明错误已经修复。VerificationProbe 只识别检查定义；它不替代 Runtime 的 current-world PASS receipt，更不赋予普通 final 自动关闭持久任务的权利。

**GC 的关键是引用语义，不是再调一组衰减权重。** Context GC 负责可逆迁移和有界工作集，Storage GC 才执行永久删除。存储根应包括活跃/固定/持久记录，以及 TaskAnchor 明确要求保留的证据，再沿强引用求闭包。弱实体关联用于候选排序；强引用用于保存证据；PromptRequired/ResidentRequired/StorageRequired 各自只负责声明的层面。本轮未改变默认分数、TTL、代际或热度策略。

**搜索应先保证范围诚实，再讨论更复杂的召回。** 当前 catalog/inverted index、路径与实体检索、有界正文复核的组合可以继续使用。已有 Context 搜索最多读取 256 个候选 blob，每个 blob 有 1 MiB 的真实读取上限，并校验正文所有权/校验和。原始文件与工具日志以描述符检索、显式读取正文，避免历史大文本重新占满上下文。目录、路径、单行、结果集合都必须有独立上限；“扫描了一部分没有命中”与“完整范围不存在”必须区分。是否引入向量检索，应由具体漏检样例和成本收益决定，本轮没有增加向量库或重跑冻结研究。

**提交与恢复要延续现有事务边界。** 当前 Workspace 的 confined 句柄、revision 检查、effect 日志和 Core 的恢复围栏是应保留的基础。台账消失不等于进程已被清理，恢复记录也不授权重放副作用。对输出超限、I/O 错误、未知执行结果采取明确的有限结果或恢复状态，比统一吞错为“空”“已完成”更可靠。

**平台下一步仍应围绕 C0/P3 的实际会话契约推进。** 本轮订阅使用“先注册流、后取快照、按水位去重、缺口重新取快照”，没有新增事件数据库或通用调度器。提交去重仍是有限的进程内窗口，不是跨重启 exactly-once。任务/焦点快照在 Actor 内一致；审批列表来自 Core 的当前集合，响应时再次核验，不能宣称二者是全局同一时刻的事务快照。需要更强一致性时，应补明确的 Core 审批版本或事件屏障，而不是让客户端自行推断。

## 验证与边界

实际执行的命令与最终结果见下表；曾失败的针对性检查用于复现和修正，未计为通过。

| 命令 | 结果 |
|---|---|
| `cargo check --workspace --all-targets --offline --keep-going` | 通过；仍有原有的协议夹具 unused imports 与 TUI unused_mut 警告。 |
| `cargo test --offline -p agent-core -p agent-contracts -p tool-runtime --lib -- --quiet --test-threads=2` | Core 151、contracts 163 通过。tool-runtime 后续新增测试后单独复验。 |
| `cargo test --offline -p tool-runtime --lib -- --quiet --test-threads=2` | 245 通过，1 项原有 ignored。 |
| `cargo test --offline -p context-simple --lib -- --test-threads=2` | 首轮 291 通过，包含 10,000 轮工作集测试。后续新增恢复证据/GC 测试后跳过该已通过的长测试，复验其余用例。 |
| `cargo test --offline -p context-simple --lib -- --quiet --test-threads=2 --skip long_task_10k_turns_keeps_the_working_set_episode_bounded` | 292 通过；仅跳过本轮已通过的 10,000 轮长测试。 |
| `cargo test --offline -p agent-runtime --test actor -- --quiet --test-threads=2` | 72 通过。 |
| `cargo test --offline -p agent-compose --test supervision_gate --test m16_restore -- --quiet --test-threads=2` | 监督门禁 3、恢复 2，通过。 |
| `cargo test --offline -p agent-conformance --test dependency_boundaries -- --quiet` | 3 通过。 |
| WSL Ubuntu：`cargo test --offline -p agent-process --lib watchdog::tests --target-dir /tmp/context-agent-review-target -- --quiet --test-threads=2` | 8 通过。 |
| WSL Ubuntu：`cargo test --offline -p tool-runtime --lib supervision::tests --target-dir /tmp/context-agent-review-target -- --quiet --test-threads=2` | 17 通过；修正了 kill 后立即断言 zombie 的测试竞态。 |
| 捆绑 Python 执行 `scripts/doc_consistency.py` | 通过，13 份活跃文档。 |

`git diff --check` 仅报告原有 `AGENTS.md:40` 文件尾空行，本轮没有覆盖该用户修改。原审查清单中的 8 份 .NET/Avalonia 客户端文件逐项 SHA-256 核对未变。

没有执行真实 provider 调用、M15/LT-EVAL 或发布。PACKAGE-01/MCP-01 仍按仓库既有条件项处理。本轮的 Linux 进程机制测试与监督接线修复，不代表完整生产宿主硬退出、所有 PID 竞态和跨平台冷恢复验收完成；正式 B1/B2 支持声明仍应遵循现有验收边界。旧数据中已经存在的语义终态不被批量改写或复活。

Git 私有视图故意不执行仓库过滤器、不自动获取缺失的 promisor 对象，并跳过子模块递归；相关结果以原始受控视图为准，需要这些执行能力时应使用经 Core 授权的执行工具。

选择独立 Git 目录的原因是 `GIT_CONFIG` 只影响 `git config` 命令，不能让普通 diff 忽略仓库配置；过滤器本身又可以配置外部命令。参见 [Git 配置文档](https://git-scm.com/docs/git-config/2.45.3.html) 和 [Git 属性文档](https://git-scm.com/docs/gitattributes)。

后续优先沿当前 M17 路线完成生产宿主监督/恢复验收和 C0/P3 会话衔接，再根据真实漏检案例改进检索。不要将本次修复转成新的“全仓审计清零”阶段。
