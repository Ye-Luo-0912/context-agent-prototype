# 第三轮覆盖、源码身份与验证限制

本文件解释 [REPORT.md](REPORT.md) 的证据深度，不增加执行队列。任务顺序仅在 [NEXT_TASKS.md](../../NEXT_TASKS.md) 顶部。

## 基线与方法

- 开始时 HEAD：`685b6bbb29275bc8ec73ce6625a94567a8b8d23d`；124 个已跟踪修改文件、20 条未跟踪状态记录。以工作树为准，没有 checkout/reset/stash 或覆盖用户改动。
- [source-start.json](evidence/source-start.json)：通过 `rg --files` 在 crates/clients/apps/scripts/.github 中盘点源码、测试、构建文件，另含根 Cargo.toml/Cargo.lock/AGENTS.md。共 507 个文件、298,469 行；其中 452 个 `.rs/.cs/.axaml`。包含测试/夹具；不是“全仓产品代码行数”。
- [lexical-inventory.json](evidence/lexical-inventory.json)：对 505 个适用文件扫描失败出口、整文件读取、无界队列、生命周期操作、clone/collect、TODO 类标记。扫描结果只是导航，不代表每个匹配都是问题。
- 主 Agent 深读 Context/GC 与跨域调用；两个只读子 Agent 分别检查执行恢复、成本/接入，均已结束且未修改文件。主 Agent 对入选发现复读关键调用点，对九个 Context/成本边界执行隔离反例。
- [source-end.json](evidence/source-end.json) 与 [source-drift.json](evidence/source-drift.json) 记录结束时源码身份和期间漂移。核对对象是开始清单；本轮新报告/探针在 docs/reviews 下，不属于产品源码漂移。

## 分区覆盖

| 分区 | 深度 | 具体检查 | 未覆盖/不外推 |
|---|---|---|---|
| context-simple | 关键链路深读＋反例 | ingest/maintain、四种 owner、required/foreground、终态判定、scope close/retire、full/minor GC、store merge/删除与恢复校验、Catalog 导航 | 没有逐行审完所有评分与全部测试；没有开启 GC 策略或排序算法研究 |
| context-baselines | Rolling 状态/压缩路径深读＋反例；Append/shared 定向 | source cap、partial fold、失败还源、退避身份、预算与维护报告 | 非零 Dynamic 预算、完整跨恢复成本窗口没有正式验收 |
| context-contextcore / agent-context-service | 适配、wire 和处理器深读＋真实子进程 | checkpoint 根解析、protecting 默认方法、reconcile、Admit、旧 checkpoint 正文读回 | 没有声称远端 ContextCore 实现或全部进程故障矩阵通过 |
| agent-runtime | Actor 生命周期/恢复/GC/checkpoint/冷查询深读；prompt 定向 | 停靠事务、单槽归属、代际检查、restore 安装、task completion、日志查询等待、正文覆盖与低权限呈现 | 新并发窗口仅源码核实，未执行 gated actor 回归；未跑完整 actor/turn suite |
| agent-core / storage / workspace / process | 相关契约与调用方定向 | Core 权威与事件边界、保留 checkpoint 根枚举、journal writer/tail、恢复来源授权；进程 adapter 使用既有 ProcessHost | 未重跑 B1/B2、崩溃矩阵、Windows/Linux 监督清理或所有文件系统攻击案例 |
| tool-runtime / capability-process | 词法全量＋当前证据路径定向 | context.manage 查询/指令分工、搜索 coverage 正文、读区间/工件续页、原生工具事实 | 不是全工具逐行安全认证；未运行真实外部能力或新插件 |
| provider-openai / compose / eval | usage、失败/重试、压缩与预算路径深读 | 双协议出口、Unknown 保留、attempts/retries、维护配置、主/维护 lane、评估汇总 | 未运行本轮本地 SSE 回归；未调用供应商或测缓存账单，不确认价格收益 |
| protocol / host / .NET / desktop / TUI | 活跃接口和长期事件流定向 | snapshot、连接/重连、事件积压、GUI 有界渲染队列、成本累计；宿主/CLI 的 compose 路由 | 未打开 GUI 手工操作或跑 .NET suite；未全覆盖 Avalonia 视图、打包或远端会话 |
| contracts / conformance / scripts / CI | 相关类型深读；其余词法/清单 | ContextEngine 默认方法、materialization/usage/恢复契约与现有文档门 | 没有全仓 conformance/CI PASS 声明；未改变 release/tag、冻结证据或工作流 |

## 反例证据

探针使用当前源码 crate 与临时测试 store；不会读取工作区真实运行数据或供应商凭据。

| case | 观察到的实际结果 | 证明范围 |
|---|---|---|
| pending_foreground | fetch 有正文，foreground=0 且 Missing | Pending 位置解析不一致 |
| warm_recall_budget | 连续三次召回 0，目标仍 Warm | 无效候选消耗数量额度；用预算 1 的最小例，默认 8 同结构扩大为八个候选 |
| retirement_ring | 513 次 scope 循环后 first=true/latest=false/len=512 | 环的裁剪方向错误；完成语义的后果另由调用链证明 |
| stored_materialization_metadata | fetch=Pinned/Session，required=Working/Task | 当前 owner 与材料化元数据分叉；通过合法 checkpoint 设置当前 owner |
| retired_scope_recall | reactivated=1，自己的 checkpoint restore 报 missing scope | 真实外置/close/retire/root recall 的组合不变量破坏 |
| store_outage_growth | Pending 固定 3，Warm 3→17→56，背压持续 true | 真实 6/20/40 轮输入的引擎积压；Runtime 未消费 flag 由源码证明 |
| service_recovery_roots | Simple roots=1、adapter roots=0、删除 1、旧正文不可读 | 真实当前服务进程的保护契约漏接；不等于全 Runtime 冷恢复 E2E |
| compatible_decisions | 五秒超时记录进入 Superseded | 两个真实兼容指令的错误永久终结 |
| rolling_same_request_backoff | 两次本地失败调用的 source/folded_items 完全相同 | 真 Rolling 的退避失效；未付费，不量化费用金额 |

五个仅源码结论：R3-09/10 的 Actor 并发窗口、R3-11 的整文件尾查、R3-12 的失败 usage 丢失、R3-14 的 GUI 成本事件丢弃。它们的拟议回归明确写入对应任务，不能以本轮 probe exit 0 替代。

## 实际检查和范围

完成服务二进制定向构建、九例隔离探针、仅探针格式化。起止 507 个文件 hash 对照无漂移；现有文档门 `OK (13 live docs, links and state agree)`，四份当前文档 `git diff --check` 通过，新报告包相对链接无缺失。部分前期读取命令因 PowerShell 不支持 Bash 花括号、输出截断或一次行号范围超过 EOF 被纠正后重新读取；没有将这些工具错误当成源码缺陷。

没有完整 Rust/.NET suite、真实 provider、远端 CI、发布、提交或推送。历史回执记录的通过数保留为当时事实，不作为这份未提交树的新验收。维护性建议均附具体失效路径，不把“大文件”或“有重复”单独列成 P1/P2。
