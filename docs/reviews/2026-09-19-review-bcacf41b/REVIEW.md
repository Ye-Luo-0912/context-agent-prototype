# 新分支续审：长流程恢复、语义生命周期与耐久测试

**仓库**：`Ye-Luo-0912/context-agent-prototype`  
**分支**：`codex/runtime-endurance-full-plan`  
**固定 SHA**：`bcacf41b9104db6ebada7adfc2a95de5e341f49b`  
**父提交 / 查询时 main**：`24c354cb1f7507efa990e2b10bfef9b185e75d5d`  
**提交时间**：2026-09-18 19:06:28 UTC / 2026-09-19 04:06:28 Asia/Tokyo  
**范围与出处**：[COVERAGE.md](COVERAGE.md)、[SOURCE_MAP.json](SOURCE_MAP.json)

## 结论

新测试有真实工程价值：回执描述了输出上限失败后的同任务冷恢复、多次反馈、真实工具执行和应用负载；人工修复也被明确标注。应保留这些进展。与此同时，不能把“各段 turn_completed”“人工修复后的应用 oracle 通过”或“独立应用控制器运行90分钟”合并成所有 Runtime 故障与长期成本已经验收。

当前优先收口是 **BR1 冷页语义意图的目标集合/时序** 与 **BR2 失败检查点的捕获事实**。BR3/BR4修正本次新加收敛策略的边界，BR5–BR7使测试控制器自身的清理、预算和证据保全可依赖。GUI仍后置，不另建调度器、记忆系统或评测平台。

本轮比对了38个变更路径，实际读取22个不同源码/配置/文档的全文或区间，**不是全仓逐行审完**。本地未执行Rust/.NET回归；七项离线机制检查详见 [MECHANISM_RESULTS.json](MECHANISM_RESULTS.json)。这些检查是明确标注的控制流模型/局部函数转写，以及受控的Popen与临时文件实验，不是仓库端到端复现。

## 当前证据状态

新SHA的GitHub Actions查询返回0个run。工作流只对main push与pull_request触发，所以分支push没有run不等于CI失败；同样也不能借父提交的绿色结论。应在合入前验证这个确切SHA。

`RUN_2026-09-19-FULL-PLAN.md`记载156个主模型请求、382个工具尝试、约$1.04的peak token-cost estimate，以及90分钟/108,000条记录的soak，状态为`COMPLETE_WITH_MANUAL_REPAIR`。这份数字是**已读回执的报告值**，不是本轮从原始SSE和应用源代码独立重算的账。引用的`target/.../full-campaign-receipt.json`在该SHA的同一路径返回404；不得由此推断用户本机没有证据。

L1回执也区分了模型贡献与控制器修复：输出上限后恢复同一任务，最后的非法`process.run`参数受到正确拒绝；控制器另补应用校验、修复生成的测试并执行验收。`25/25、16/16、18/18`分别来自不同层次的最终验收记录，不构成纯自主成功。

## BR1 — 延迟语义意图不是单目标，但首次命中就整体消费

**建议P1；继承自主分支父提交中的H2实现，不是重复报告“完全没有冷页语义更新”。**  
入口：`crates/context-simple/src/gc/reachability.rs`，`record_cold_supersession_intent`、`apply_cold_semantic_intents_on_install`、`apply_cold_intent_to_entry`。已核对635–965及否定保护180–285。

### 机制一：目标集合没有走完

`PendingColdSemanticIntent`是按task/entity/requirement或verification probe匹配的谓词，而非一个具体target ID。安装一批卡片时，只要任意entry返回匹配，`consumed=true`，整条意图就不再保留。其他符合相同条件、仍处于后续冷页的条目因而错过更新。

反例：同一任务两条旧决策A/B都满足同一次明确撤销，A在第一安装批、B在第二批。第一批把A终结并清掉意图；第二批B仍Live。对应的非相同故障正文、同probe的旧Error也可以构成多目标验证场景。

### 机制二：缺少因果边界，会处理后来才出现的要求

意图只携带`by_id/task_id/entities/content/probe`等字段，不绑定当时的目标集合、卡片版本、目录代际或创建上界。某意图未命中时长期保留，随后同任务创建的新决策只要与它匹配，冷安装时也可能被这条旧意图终结。

例如撤销旧文件要求时，只存在无关pending卡片，撤销谓词被保留；后来用户重新要求使用同一文件，新决策外置并读回后，旧谓词仍可命中它。模型已接收的新要求不应被更早的撤销重新作废。

### 机制三：截断前后的否定规则不同

冷意图保留输入前4000字符，而否定/保留检查在安装时对这个副本执行。完整输入的后部若有明确“不撤销/保留”修订，已加载路径使用完整消息能够保留，截断后的延迟路径却可能丢掉该否定。局部探针使用4322字符输入验证了这个条件差异；未执行真实引擎长输入回归。

Verify分支还有同根残余：任务/probe形状匹配后，即使`has_matching_verification_evidence`因证据未加载而返回false，函数仍返回true，可能消费尚未完成的义务。形状匹配与状态成功结算不能共用一个布尔值。

### 最小修复与停止条件

将意图变为有界、持久的**目标视图内更新义务**：绑定目标或目录视图/因果上界，保留尚未处理的位置，成功处理一个目标不能清掉其他目标。已终态的幂等目标、未解析目标、证据暂不可验证要有不同结果。需要全文语义时使用现有可取回来源，或以明确受限的结构化修订取代截断再判定；不以有损文本作为终态证明。

不要求全历史常驻、不无限pin，也不取消已有proof/liveness检查。定向回归覆盖两个匹配页、后创建目标、旧意图无匹配、by证据暂不可读、输入尾部否定，以及中途checkpoint/restore。

## BR2 — `captured=true` 可能只是“尝试过捕获”

**建议P2，静态组合反例，需在仓库执行F10+F12验证；不是已经观察到数据丢失。**  
入口：`actor/lifecycle.rs: resume_failed_turn_checkpoint`、`actor/safepoint.rs: safe_point_resume_commit / continuation_durability_gate`、`actor/maintenance.rs`。

当`gc_work`占用边界通道时，新分支执行：增加`FailedTurnYield` debt，调用`safe_point_resume_commit`，然后无条件设`captured=true`并停放续接。

但capture函数返回`()`，遇到已有`checkpoint_write`或`checkpoint_prepare`就提前返回。调用者不能从返回值分辨“确实创建了包含新debt的快照”还是“由于旧工作在途，尚未捕获”。

目标交错：旧任务完成GC占lane；新任务首次只读工具把已有anchor debt送进旧prepare S1；后一个模型请求失败。失败debt在S1冻结后新增，capture因S1存在而无动作，captured却为true。释放旧GC/S1后，旧ACK只退休旧debt；续接不再新建失败快照，durability gate因剩余FailedTurnYield而围栏。

现有测试分别覆盖“有旧prepare但lane空”和“lane占用但没有这个额外prepare”，不等同于它们的组合。`scenario.json`中的C7正是F10+F12。

**修复**：让capture返回`Captured{sequence, debt_basis}`、`Deferred`或等价类型；只有相应失败义务确实进入快照才标记捕获。旧prepare完成后按当前义务重新驱动，同一序列只能退休自己的debt。保持单lane、取消代际、终态与严格持久门禁，不能靠清空debt消除围栏。

**回归**：复用`failure_resume.rs`的OccupiedBoundaryContext与read-first模型，组合两条既有轨迹；不取消时应取得新快照后TurnFailed且不多余RecoveryRequired；取消时只一个TurnCancelled，不发布迟到TurnFailed。再次恢复同TaskId，指令身份和工具结果不重放。

## BR3 — 覆盖表饱和不能证明新读取重复

**P2，收敛诊断错误；不直接删除正文或授权完成。**  
入口：`execution/state.rs::record_observation_evidence`，已核对1800–2160。

新实现使一个path@revision证据行保存多个窗口，避免A→B→A交替读假装推进，这是正确方向。但`coverage_saturated`定义为“新窗口不在已知集合，集合已到上限”，随后被并入`equivalent_observation`。同版本、current的未见窗口会直接返回`Repeated`。

随后`update_convergence`及prompt警告可能把首次阅读新区域描述成重复已知内容。应区分“已证明重复”和“本地摘要无法继续记录/比较”。保留容量边界，允许先做会缩小集合的合法合并；未能表示的覆盖返回Unknown/UntrackedNovelty等，不伪造covered。窗口摘要不应直接驱动强制完成。

回归用实际生产cap：填入互不相交窗口，再读一个新窗口、一份已知窗口、一段可桥接窗口以及新revision；不提额、不删除正文，用事件和下一次prompt核对分类。

## BR4 — 预算最后一轮的纯文本收尾仍可能被工具缺失挡住

**P2，条件性控制流缺口。**  
入口：`actor/model.rs:735–940`与`surface.rs:130–310`。

`force_budget_finalization`清空工具schema/mandatory，表示最后一轮只产生普通答复。但更早计算的`unavailable_must`仍存在，随后的拒绝分支只豁免`completion_repair_terminal`，不豁免budget finalization。于是已选择纯文本模式，仍因某个不再准备调用的必需工具而拒绝发出该轮请求。

将文本收尾原因收敛到一个显式模式，由模式决定该轮是否还存在工具执行义务。普通执行轮继续拒绝不可用MustSurface；文本收尾仍不得发工具，不得当作任务验收通过。回归固定总请求上限与缺失工具，验证可以报告受限结果，但Core权限毫不放宽。

## BR5 — 测试runner超时后没有拥有并完成子进程清理

**P2，测试基础设施，不归因到Runtime本身。**  
入口：`scripts/runtime_endurance_incremental_runner.py`。

`Popen.communicate(timeout=780)`抛异常不会自动kill；finally只关闭HTTP server并写usage账，未terminate/kill/wait子进程。正常Runtime自带720秒超时有助于正常退出，但测试器恰恰也需要处理宿主失灵、I/O卡住和键盘中断，不能依赖被测方自救。结束的metadata/summary又位于finally之后，超时会跳过。

本轮实际启动了一个与仓库无关的临时Python child，communicate超时后`poll()`仍为None；探针自己的finally随后kill并reap，未留下进程。这是Popen契约检查，不是Windows Runtime漏进程复现。

另一个出口问题：runner记录child exit或`protected_unchanged=false`后只print，不将失败反映到自身退出码；正常完成这些Python语句可exit0。自动编排不能只看runner进程码就宣布该阶段成功。

**修复**：一处无条件finally拥有child/relay的结束顺序；正常/异常/取消都写终态回执。按身份停止自己的进程树，不杀无关进程；清理未确认独立报告。对子进程失败、保护文件变化、预算或证据不完整分别给明确退出码/状态。错误不应伪装成应用验收失败，也不能成功掩盖保护边界失败。

## BR6 — relay是“账后停止”，不是完整的campaign硬额度

**P2；本轮没有证据表明已完成实验实际超支。**

同一runner的付费保护有四处不完整：

1. 每个segment重新初始化spend=0，不读取scenario规定的全campaign决策/attempt/token/时长余额，resume不会继承这些硬余额。
2. 只看已结算spend是否达到阈值，没有为正在接受的下一请求预留费用。即便串行，0.99<1.00仍会放行估计0.10的请求；并发请求又可同时看到旧余额。
3. usage parser接受“仅有任一计数字段”的字典，随后把缺input/output/cache字段用`or 0`补齐；顶层usage存在不等于账目完整。
4. 上游open的非HTTP异常直接返回502，部分流异常也可能绕过用量结算；这些不确定attempt没有统一进入Unknown账目。两段正常SSE之外的路径同样需要记录。

当前hardcoded rates和约$1.04是特定runner估计，不是已独立复算的账单。T8走Chat，runner强制Responses；字段层级与包含口径必须按实际端点适配，不能因为model名称相同就共用默认提取。

**修复**：复用现有账目为整个campaign持久记录committed/reserved/unknown，每个真实attempt先预约有限额度、结束时统一结算。缺测值不补零，拿不到可靠上界就停止新的付费尝试。预算受理还要校验CLI数值有限且非负，采样率/时长/轮数等实际配置写入回执。不靠调大额度让测试完成。

需要本地受控relay回归，不需要真实付费：缺usage、部分usage、read失败、429重试、两个并发request、跨segment冷重启、临近上限的最后请求。

## BR7 — 重跑setup/L0会覆盖刚完成的应用和baseline

**P2，测试产物保全。**  
入口：`scripts/runtime_endurance_full_campaign.py::setup/l0`。

campaign目录固定，`setup`用`exist_ok=True`并无条件重写种子FILES和baseline-lock；`l0`首先调用setup。第二次运行会把已改的engine/store/queue等覆盖回v1，但后来新增模块不被移除，形成混合工作区；旧segment证据仍在，baseline却重新显示PREPARED和0 calls。

采用唯一campaign ID和排他创建；resume只验证已有身份，不重铺seed。显式重置只能由用户选择新的目录或执行独立、清楚的破坏性操作。回归再次setup应拒绝，所有既有文件和baseline哈希不变。

## 证据与成本结论的边界

详见 [TEST_EVIDENCE.md](TEST_EVIDENCE.md)。这次不把真实试验贬成无效：失败后同任务恢复、模型产生代码、控制器找出并修复剩余缺陷，都提供了有效信息。但要分别记录：

- 自主阶段结果与人工修复后的应用结果；
- Headless回合终结与operator任务验收；
- 应用worker/负载controller的90分钟与Rust Runtime故障覆盖；
- 本地回执与原始证据本轮是否可复查；
- endpoint接受、server hit、金额正规化、配对净成本改进。

T8已有真实Chat命中，不能继续标成“从未做过命中实验”。但它没有配对比较同等完整任务，也未正规化美元价格；耐久runner另走Responses。T8文案将`16896`称为cross-run first-request hit，表内该数实际是11轮总和，首轮是`1536`，应修正标签。

## 下一阶段建议

保留现有三线，按 [NEXT_ACTIONS.md](NEXT_ACTIONS.md) 收口：B先做逻辑语义义务，A做真实捕获与收敛事实，C把测试器和用量结算合成可信边界。然后将一条同任务轨迹跑通现有故障组合与交付验收；无需为每个小修复再跑90分钟/再买一个满额模型segment。

维护性看同一事实有几个定义：capture的结果用类型返回；语义更新有目标/时序/结算；coverage溢出不造知识；controller结果、账目和进程归属在共同出口结算。不要新增第二个任务权威，也不要把全部history重新塞回默认文档与prompt。
