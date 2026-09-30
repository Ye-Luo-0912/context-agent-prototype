# Terminal-Bench 小批量高难度配对评测方案

状态：**TB-4_B_BOUNDED_PILOT_COMPLETE_STOPPED**。2026-09-21–22 已完成选题、协议、Runtime 外置
状态目录、Harbor 适配器和零供应商预检，并完成 A 阶段三题各一对、B 阶段三题各一对的有界
Rolling/Dynamic pilot；所有槽位 reward=0，结果只作诊断 pilot。
官方 oracle/no-op 校准已完成；每槽最多 20 个模型回合，低于方案的 240/400 回合上限，结果只作
诊断 pilot，不是完整官方评测。
本地源码基线为 `ece82e04fee63625675248fb80d638b02cdcdd26`；开窗时须另记实际源码、
二进制和适配器摘要。本方案不延长 V5 的原预算，也不改变其冻结结果。
实施回执：[TB2_RECEIPT_20260921.md](TB2_RECEIPT_20260921.md)。

## 1. 本轮要回答的问题

让完整 Runtime 在外部定义的困难任务中自主读代码、修改、构建、调试和交付，观察：

1. 同一模型、工具和资源下，Rolling 与 Dynamic 上下文配置分别完成哪些题？
2. 未完成时，时间和费用耗在理解、重复读取、无效操作、长工具等待还是上下文缺口上？
3. Runtime 是否丢失工具结果、误处理失败、错误恢复或提前结束任务？

六题是**按机制相关性选择的诊断样本**，不是随机样本，也不是官方最难六题排名。
作者的专家工时只作为难度线索；没有读取本模型在这些题上的成绩来筛选题目。
任务涉及应用自己的恢复/GC，不直接证明我们的 Context GC 或 checkpoint 正确；
后者需要 Runtime 轨迹和另列的故障变体提供证据。

## 2. 数据集与六道主选题

采用 [Terminal-Bench 4.0.0](https://www.tbench.ai/news/terminal-bench-4-0)，固定官方仓库
`harbor-framework/terminal-bench` 的提交 `452bf305c6daa62fc59061d22133a7cbc7c1572e`。
2026-09-21 查询确认 `v4.0.0` 指向该提交；八道候选的 `instruction.md` 与 `task.toml`
均重新下载，并按 Git blob SHA-1 与官方树核对。题目树和关键文件身份、资源、顺序及预算见
[selection.lock.json](selection.lock.json)。这固定了题源；镜像 digest、Harbor 版本与动态依赖
还需执行前冻结，不能仅凭一个 Git SHA 宣称环境可复现。

下表资源均来自该提交的 `task.toml`；所有主选题 agent 时限为 **28800 秒／8 小时**。
CPU、内存是任务环境声明值，不是整套多容器系统的总占用。正文没有复制解题步骤。

| 顺序／阶段 | 官方题目 | 任务与选择理由 | 专家工时 | agent CPU／内存 | 裁判时限 |
|---|---|---|---:|---:|---:|
| 1／A | [wal-recovery-ordering](https://github.com/harbor-framework/terminal-bench/tree/452bf305c6daa62fc59061d22133a7cbc7c1572e/tasks/wal-recovery-ordering) | 修复多模块 WAL 存储与恢复；跨模块理解持久化、可见性、并发顺序和对象隔离，贴近本项目恢复与提交边界 | 6 h | 2／4096 MiB | 1800 s |
| 2／A | [mvcc-lsm-compaction](https://github.com/harbor-framework/terminal-bench/tree/452bf305c6daa62fc59061d22133a7cbc7c1572e/tasks/mvcc-lsm-compaction) | 根据事故材料定位 C++ MVCC/LSM 可见性缺陷，修复并补确定性回归；检验在保留正确性时回收历史的推理能力 | 4 h | 2／4096 MiB | 900 s |
| 3／A | [session-window-debug](https://github.com/harbor-framework/terminal-bench/tree/452bf305c6daa62fc59061d22133a7cbc7c1572e/tasks/session-window-debug) | 修复事件窗口合并、输出与状态回收；需求分散在设计和源码中，适合观察跨文件证据保持与误删状态问题 | 8 h | 2／4096 MiB | 300 s |
| 4／B | [payments-pipeline-fix](https://github.com/harbor-framework/terminal-bench/tree/452bf305c6daa62fc59061d22133a7cbc7c1572e/tasks/payments-pipeline-fix) | 优化支付 worker 冷启动和滚动部署，通知不能丢失或重复；检验长工具、持久状态和外部效果语义 | 2 h | 4／8192 MiB | 600 s |
| 5／B | [rs-archive-clone](https://github.com/harbor-framework/terminal-bench/tree/452bf305c6daa62fc59061d22133a7cbc7c1572e/tasks/rs-archive-clone) | 从允许的黑盒探测重实现归档工具，兼容正常和错误行为；长假设链、实验记录和反例累积压力较大 | 16 h | 4／4096 MiB | 1200 s |
| 6／B | [live-database-cutover](https://github.com/harbor-framework/terminal-bench/tree/452bf305c6daa62fc59061d22133a7cbc7c1572e/tasks/live-database-cutover) | 持续业务访问下从 MySQL 切到 PostgreSQL，保持行为、数据与性能；最接近持续开发和运行中验证的综合任务 | 8 h | 16／16384 MiB | 600 s |

支付题专家估时较低，但保留它是为了覆盖冷启动、外部通知和多服务流程；归档题是单文件
交付，保留它是为了覆盖长时间黑盒实验，两者不冒充跨文件修复题。

两个**开窗前替补**：`risk-scorer-replay`（2 CPU／4096 MiB，专家 4 h，裁判 300 s）用于
支付题环境无法成立时，覆盖旧行为重建、来源材料和确定性审计；`distributed-dedup`
（8 CPU／16384 MiB，专家 10 h，裁判 1800 s）用于在线迁移环境无法成立时，覆盖
分布式去重、等价性与资源约束。替补不是等价任务，必须更换协议身份并披露覆盖变化。
只能在任何主选题模型调用前，依据环境预检结果决定替换；看见失败成绩后不得换题。
若两个备选环境也不合格，保留该槽为 NOT_RUN，不临时挑容易题。

## 3. 配对条件与结论边界

每题各跑一次 R 与 D，共 **6 对／12 次**：

- R：本项目 Runtime，`--context=rolling`。
- D：同一 Runtime 二进制，`--context=dynamic`。

两组固定相同的模型 serving、协议、思考模式、采样参数、每请求输出上限、发送预算、
工具集合、Core grants、官方任务镜像、CPU／内存、初始数据和总费用上限。相邻配对运行，
顺序预先交替：R→D、D→R、R→D、D→R、R→D、D→R；不并行竞争本机资源。
模型若不支持 seed，如实记 `unsupported`；不声称单次配对消除了随机性。

模型建议沿用现有 `deepseek-flash` serving，执行前核实可用性、协议、上下文容量与价格。
本轮不混入第二个模型。若改变模型、提示、工具或预算，则创建新协议，不能拼成旧成绩。
两组均显式固定非思考模式（前提是 serving 确认支持），单请求输出上限拟为 8192 token。
主请求有效发送上限拟为 32768 token，并按框架实际装箱口径验证；不能只设置一个环境变量
就声称已形成可比较的 32K 窗口。模型支持容量、Runtime 装箱预算和最终 wire 计数分别记录。

**首轮两组都禁用模型维护调用**：
`MAINTENANCE_MAX_CALLS_PER_MAINTAIN=0`、`MAINTENANCE_MAX_TOKENS_PER_MAINTAIN=0`。
当前 [build_context_engine](../../../crates/agent-compose/src/lib.rs) 可构造两种引擎，
零预算不会挂载模型压缩器；但 Dynamic 的正数逐 pass 调用/token 上限尚未接入。
因此本轮比较的是两个明确受限的上下文配置，不能称为默认产品完整模式对比，
也不能据此评价模型摘要/episode distill 的收益。若后续要开启维护，先完成统一额度接线，
再作为新的实验条件，维护用量与主请求一起入账。

每个 trial 使用全新容器、工作区、TaskId、Context store 和 checkpoint；一组不能读取
另一组轨迹、补丁、测试结果或状态。供应商缓存无法保证清空，记录 hit/miss/unknown
和配对顺序；缓存命中及费用差异只作观察，不据此宣称某布局造成降本。

这能比较上下文策略在本 Runtime 中的表现。两组共同失败仍可能来自模型、工具、
Runtime 共同路径或题目环境；首轮没有外部参考 agent，不能把共同失败直接归因于模型。
外部 agent 对照与完整维护配置均不加入这 12 次的范围。

## 4. 预算与阶段停止条件

这是**预算受限的项目诊断评测**。官方题面和 8 小时配置原样保留；外层 pilot 控制器
按下表提前截止，并把实际截止明确告知受试 agent，不制造题面时间仍可用的错觉。
不得改官方裁判、数据规模或资源阈值以适应本机。提前截止的成绩不得当作官方全预算结果。

| 每次 trial 上限 | A：前三题 | B：后三题 |
|---|---:|---:|
| agent 实际运行墙钟 | 2 h | 4 h |
| 主模型决策 | 240 | 400 |
| 供应商 attempts（含重试） | 280 | 460 |
| 工具尝试停止阈值 | 800 | 1400 |
| 累计输入 token（每次请求均计） | 800 万 | 1600 万 |
| 累计输出 token | 22 万 | 44 万 |
| 估算供应商费用 | USD 2 | USD 4 |
| 维护模型调用 | 0 | 0 |

任一上限先到即停止新受理，结算已知用量并有界收尾；不能按 segment 重新计算额度。
attempt、token 和金额在请求边界预约，未知用量不得补零，未结算预约保留并停止新增请求。
现有 runner 的工具数由完成事件约 100 ms 轮询得到，不是严格受理上限；接入时须同时记录
受理数、完成数、在飞数及停止后的超阈值量。未实现严格准入前，本表工具项只称停止阈值。

A 阶段 6 次至多 **USD 12／12 agent 小时**；B 阶段 6 次至多 **USD 24／24 agent 小时**；
合计 **USD 36／36 agent 小时**。这是支出和时长上界建议，不是通过题目所需费用估计。
官方 build、collect、verifier 时限另计，故不是整个 campaign 36 小时保证；容器、磁盘、
网络或远端主机费用不包含在 USD 36 中。本方案不创建付费主机。执行前冻结当日价格表、
估价算法和明确授权额度；额度与价格缺项不能标 READY。

A 结束先核对协议、事件和账本；合格后再进入 B。如果 A 六次全未通过且均已到预算上限，
按预设规则停止扩展并报告“当前模型/预算下出现地板效应”；保留 0/6 与原因，不靠换题、
提示解法或同窗口提额补分。Runtime/适配器/清理错误则立即暂停后续受影响 trial，先归因。
只有某道题正常失败不触发提额，也不触发人工修复后回填成绩。

默认无完整 trial 自动重跑；传输层有界重试计入同一次 attempts 和费用。环境失败的重跑
另建身份，保留原记录，不能删除失败后称首次通过；新增花费仍需落在已授权窗口内。

## 5. 接入方式与当前缺口

采用 Harbor 的 [installed custom agent](https://docs.harborframework.com/core-concepts/agents/custom-agents)：
在题目的 agent 环境内运行本项目 Linux 二进制，Harbor 负责环境生命周期与官方裁判；
RuntimeActor 继续编排回合，Core 继续审批、提交和恢复。适配器只承担安装、启动、
输入传递、日志/用量转换和有界收尾，不自行实现第二套工具推理循环。

已有可复用入口：[agent-tui args](../../../crates/agent-tui/src/args.rs) 提供 `--work`、
`--prompt=-`、`--context`、`--max-rounds`、`--timeout-secs`、`--grant-file` 和 JSONL 输出；
`--max-rounds` 是单 segment 约束，不能替代跨段账本。进程退出 0 或普通 final 都不等于
题目通过，也不等于 OperatorClosureOnly 下的持久 TaskCompleted。

实施时保留题面 `/app` 路径及原服务拓扑，不能把项目工具悄悄接到 Windows 宿主工作区。
为编译、测试和容器内服务访问声明两组相同的能力/grants，实际运行 schema、只读限制与
长进程轮询/取消预检；模型代码不能获得宿主 Docker socket 或读取控制器证据目录。
需要访问题内多服务时使用官方环境已有路径，不能用 Core 授权推导宿主权限。

六道主选题均配置 separate verifier。必须完整保留官方 artifact/collect 流程：支付题有
Kafka 状态收集，迁移题有跨服务结果、数据库/Redis 状态与 API patch 收集，不能仅复制
最终源码后运行一个本地测试来代替。官方说明见
[Separate verifier](https://docs.harborframework.com/core-concepts/tasks/separate-verifier)。

当前 CLI 状态位于工作区 `.focus-agent`，WAL 等题又会收集整个 `/app`；因此适配器必须
在预检中证明 Runtime 私有状态、凭据与轨迹不会混入提交。若现有入口不能安全分离，
先做最小的状态目录/导出接线并回归；不能发明已有 `--state-dir` 参数，也不能在取证前
清空状态来掩盖泄漏。候选产物映射变化必须记录，官方任务正文和裁判保持固定。

**当前尚无本方案的 Harbor 适配器或 READY 回执**。既有 SWE-bench 导入器和 V5 runner
只能提供部分参考，不构成这六题已接通的证据。本次是入口与相关区段的局部源码核对，
不是全仓审查，也没有执行 Rust、容器或官方题目的测试。

## 6. 零供应商预检与污染隔离

2026-09-21 本机检查：Windows PATH 与默认 WSL 中均未检出 `docker`、`podman` 或
`harbor`；Windows 主机报告 16 逻辑处理器、约 31.3 GiB 内存。这不能证明 Linux 容器
可用或满足迁移题整套服务需求。迁移题还声明 40 GiB verifier 存储，支付裁判需要
6 CPU／12 GiB；必须按完整拓扑实际预检，不能只看主机内存或将限制调小。

执行前依次完成：

1. 锁定 Linux x86_64 执行环境、Harbor 安装版本、全部镜像 digest、资源限制和可用磁盘；
   支持这些任务的 schema 版本、separate verifier、多容器 collect，缺一项即 NOT_READY。
2. 在每题独立校准环境运行官方 oracle 正对照和 no-op 负对照，保存 reward 与原始日志。
   预期 oracle 通过、未修复基线不通过；不符则先调查环境或题目，不修改裁判制造通过。
   校准产物和卷绝不复用于模型运行。这些控制不消耗模型额度，但消耗机器时间。
3. 用确定性本地 mock 通过真实 CLI/Runtime/Core 执行读、写、长工具、一次失败和有界停止，
   证明工具不越出题目环境、用量计数、退出状态和产物收集均能贯通。mock 成功不算模型成绩。
4. 验证两组有效配置只存在声明的 Context 差异；工具 catalog、权限、发送上限、零维护、
   预算触发、未知用量、在飞清理、日志缺段、无 reward、artifact 缺失都有明确结果。
5. 冻结 run manifest，填写模型/价格/授权/镜像/二进制摘要，才允许状态变为 READY。

选题阶段已接触部分官方 README 的难度与解题说明，因此本设计会话不能作为受试 agent
的初始上下文。正式 trial 只接收官方 `instruction.md`、官方 agent 环境及统一的预算提示；
不传入本方案的分析、README 解法、`solution/`、`tests/`、oracle 日志或其他组轨迹。
题面允许的源码/设计文档/黑盒探测保留。题库快照和判卷文件只在控制器端保存；
agent 网络不用于查找本题解答。正式结束前不把隐藏裁判反馈送回模型继续修题。

## 7. 判定与证据

每次记录三条独立轴，避免“失败都算环境问题”或“退出成功就算交付”：

- `official_reward`：原始官方 reward，缺失记 null；按题目原定义报告，不自设部分通过分数。
- `stop_reason`：正常结束／预算／时限／provider error／Runtime error／人工停止。
- `evidence_validity`：VALID／ENVIRONMENT_INVALID／PROTOCOL_INVALID／INCOMPLETE。

正常的功能失败、代码写坏、agent 引起的进程/内存耗尽、超时及预算耗尽均保留在已运行
trial 的结果中；不能重分类成环境无效。构建/依赖/服务在 agent 启动前已失败、校准
oracle 同环境失败、宿主资源不达声明、官方 collect/裁判基础设施损坏，才有证据依据
记 ENVIRONMENT_INVALID。我们适配器丢事件、错误导出、错误授权或遗漏预算是本系统问题，
单列 PROTOCOL_INVALID/INCOMPLETE 并计入交付失败汇总，不替 Runtime 隐去。

预算截止后若能安全收集，则对最后候选执行一次官方裁判；reward 即使通过，也要保留
预算终止原因和运行完整性。进程清理未确认、账本未知或关键证据缺失时不能宣称整体成功。

报告保留六题×两组的预定表格，包括 NOT_RUN 和无效槽；另列有效配对数、R/D 单独通过、
两组都通过、都失败、R-only、D-only。小样本单次运行只给逐题观察，不声称统计显著或
全面优于另一策略；不拿裁剪后的有效样本分母掩盖环境/系统失败。

每 trial 最少保留：协议/任务/镜像/二进制身份、题面摘要、无密钥有效配置、grants 摘要、
TaskId/run lineage、完整事件 journal、工具退出与清理事实、最终 patch/产物哈希、
Harbor 原始 result/reward/verifier 日志、逐 attempt 用量/重试/缓存/预约/未知费用账本。
供应商账单与本地估价分开；主模型与维护用量分开（本轮后者应为零）。

诊断指标记录实际读取和输出字节、相同路径/范围/版本的重复读取、工具失败与重试、
首次有效修改时间、公开测试执行与通过事实、上下文 required miss、交付停滞峰值、
产物变更和验证时间线。路径相同不等于重复，文件字节改变不等于有效进展；
Runtime 的 delivery advisory 不能替代官方验收。阶段性曲线只使用公开测试和客观事件，
不在正式过程中额外调用隐藏裁判提供提示。

## 8. 实施切片与停止点

1. **TB-1：本方案与选题锁定**——本次完成。文档检查、题面/TOML blob 身份和预算加总通过；
   不代表镜像预检或评测接线完成。
2. **TB-2：Harbor/CLI 适配与零供应商预检**——实现已完成；Windows/WSL 的初始检查曾因
   缺少容器运行时和 Harbor 阻塞，用户授权安装 Docker 后，WSL Harbor、oracle/no-op 校准、
   separate verifier 和 mock Runtime 均已实测。TB-2 阶段事实保留在回执，某题环境不合格仍只记录，
   不为通过而改题。
3. **TB-3：A 阶段三题配对**——已完成六个有界 pilot 槽位；所有 reward=0，结果见回执，
   不把 20-round 诊断结果扩写成完整 240/400 回合成绩。
4. **TB-4：B 阶段三题配对**——payments、rs-archive、live 均已完成 R/D 有界 pilot；
   所有 reward=0，live verifier 暴露 MySQL-less fresh container 启动失败。20-round 结果不替代
   完整 400-decision 成绩；后续如继续必须建立新协议身份或独立 fault_variant。

12 次结束即停止范围扩张。后续取消、冷恢复、权限撤销只针对实测暴露的具体问题，
在同题的新副本建立标为 `fault_variant` 的独立实验；不篡改本轮正常 benchmark 轨迹，
不将变体与正常成绩混合。Challenges 长任务和更大 SWE-bench 样本留作后续决策。
