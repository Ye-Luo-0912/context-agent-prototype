# 可执行任务队列

有效范围见 [CURRENT.md](CURRENT.md)。本文件只保留本阶段仍需动作的任务；历史缺陷描述、验证日志和已关闭细节只链接到原回执，不复制正文。任务依据：[下一阶段审查](reviews/2026-09-14-next-stage-review-4aaa8bea/REVIEW.md)（T 编号）＋[续审](reviews/2026-09-15-continuation-review-258eb4eb/REVIEW.md)（S 编号；R1–R6 为其发现；T1–T7、S1–S4 已关闭）＋[4f6eb7ff 审查](reviews/2026-09-16-review-4f6eb7ff/REVIEW.md)（V 编号，其 NEXT_ACTIONS 与覆盖表见同目录）＋[d92564bc 审查](reviews/2026-09-16-review-d92564bc/REVIEW.md)（U 编号，其 NEXT_ACTIONS 与覆盖表见同目录）。

## 接手规则

先核对当前分支、HEAD 和未提交 diff（并行分支在飞）。MERGED 只说明代码进入目标分支，不代表 CI 或真实供应商验收通过。本轮 A=执行核心/工具，B=上下文/GC/搜索，C=平台/供应商 KV。共享 contracts/ModelInput/缓存契约由单一集成人维护。

**第十二批（`bcacf41b` 续审）已全部关闭（2026-09-19，本地集成回归全绿，回执：[BATCH12_RECEIPT](reviews/2026-09-19-review-bcacf41b/BATCH12_RECEIPT.md)）。第十一批（`d3a05d29` 续审）已全部关闭（2026-09-18，本地集成回归全绿，见下方第十一批与五份回执；CI 终验 run `35289417269` 首跑全绿）。** 第十批已全部关闭（CI 终验 run `35278745998` 与钉宽后 run `35282364426` 见 CURRENT.md）。条件任务 T8 亦已关闭（2026-09-18，授权付费实验：ENDPOINT_ACCEPTED=PASS／SERVER_HIT=OBSERVED／token 口径全落账、金额 NOT_RUN；`context_manage` 租赁打断供应商前缀复用实测成立，见 NEXT_TASKS T8 节与 [t8 证据](walkthroughs/2026-09-18-t8-kv-live.md)）；host_e2e 起始基线抖动为未定性观察（见 CURRENT.md 已知抖动记录）。历史顺序（已关闭）：T1/T2/T3 并行（第一批）；T4/T6 并行（第二批）；T5 随组合点接入；T7 收尾。每片先写用户动作与目标反例，再做实现；可维护性边界（见审查报告「四个责任边界」节）随片交付，不另起全仓重写；同一 crate 内多个切片按文件所有权串行，不并行互踩。

## 第十二批（`bcacf41b` 续审：耐久分支 BR1–BR7＋E-1 证据分层）——已全部关闭（2026-09-19，本地集成回归全绿；回执：[BATCH12_RECEIPT](reviews/2026-09-19-review-bcacf41b/BATCH12_RECEIPT.md)）

审查基线 `bcacf41b`（报告：[REVIEW.md](reviews/2026-09-19-review-bcacf41b/REVIEW.md)，任务规格：[NEXT_ACTIONS.md](reviews/2026-09-19-review-bcacf41b/NEXT_ACTIONS.md)，证据口径：[TEST_EVIDENCE.md](reviews/2026-09-19-review-bcacf41b/TEST_EVIDENCE.md)，覆盖表：[COVERAGE.md](reviews/2026-09-19-review-bcacf41b/COVERAGE.md)，机制探针：[MECHANISM_RESULTS.json](reviews/2026-09-19-review-bcacf41b/MECHANISM_RESULTS.json)）。主线：**同一项义务跨时间、分页、失败和恢复保持一致；测试控制器自身不抹掉失败或预算缺口。** 沿现有三线继续修，不重做架构，GUI 后置不变。BR 编号仅属于本报告，不重用旧 H/G/S 编号。耐久分支已有真实进展保留（输出上限失败后同任务恢复、应用负载、T8 真实命中）；COMPLETE_WITH_MANUAL_REPAIR 等分层事实不合并宣称。文件所有权：B-1=context-simple；A-1=actor/{safepoint,lifecycle,maintenance}＋failure_resume 测试；A-2=execution/state.rs＋actor/model.rs＋surface.rs；C 线=scripts（C-1 与 C-2 同文件串行）。

### B-1 — 冷页语义更新义务（BR1，P1，context-simple；先行）——已关闭（`6e993a03`）
用户动作：多页历史已外置时，用户明确替代旧要求；之后的新要求不能被更早的替代误伤。
- 现状：`PendingColdSemanticIntent`（H2 引入）保存可匹配多目标的谓词，但安装时任意 entry 命中即整条 `consumed=true`——首批终结 A、第二批的 B 仍 Live；无创建时点/目录视图绑定，未命中即长期保留，旧意图可终结后来同任务新建的同条件要求；冷路径按输入前 4000 字符截断副本做否定/保留判定，完整消息尾部修订丢失；Verify 形状匹配后即使 `has_matching_verification_evidence` 因证据未加载为 false 仍返回 true。
- 修复：收敛为有界、持久的**目标视图内更新义务**——绑定目标集合或目录视图/因果上界，逐目标独立结算（处理 A 不清 B）；未解析目标≠无目标，已终态幂等、未解析、证据暂不可验证结果分开；需要全文语义时沿现有来源引用取回或保持未确定，不以有损前缀为终态证明。不全历史 hydration、不无限 pin、不建新记忆库。
- 回归：两条同条件旧记录分两批安装必须全部更新；旧 intent 无匹配后同任务新建匹配决策不被终结；by 证据暂不可读不消费未结算目标；超 4000 字符、尾部保留修订冷热结果相同；处理中途 checkpoint/restore；跨任务与已终态负对照。
- 停止：上述反例与现有生命周期定向测试通过；不顺便改评分算法、不引入向量检索/新存储引擎。

### A-1 — 失败检查点的真实捕获结果（BR2，P2，agent-runtime；B-1 后做组合回归）——已关闭（`d1b75da5`）
用户动作：旧维护/快照在途时，新回合失败仍保住正确继续边界，status/cancel 可响应。
- 现状：失败回合遇占用 GC 通道 → 增加 `FailedTurnYield` debt → `safe_point_resume_commit()` → 无条件 `captured=true`。capture 返回 `()`，遇在途 `checkpoint_write/checkpoint_prepare` 直接提前返回——调用方无法区分"已创建含新 debt 的快照"与"旧工作在途尚未捕获"。旧 prepare S1 完成只退休自己冻结的旧 debt；续接不再补捕获，durability gate 因剩余 debt 进 RecoveryRequired（可避免的继续执行阻断；持久门禁本身不放宽）。
- 修复：capture 返回类型化结果（如 `Captured{sequence, debt_basis}`／`Deferred`／`AlreadySatisfied`）；只有对应失败义务确实进入快照才标记已捕获；旧 prepare 完成后按当前义务重新驱动，同一序列只能退休自己的 debt。保持单 lane、取消代际、终态与严格持久门禁，不靠清空 debt 消除围栏。
- 回归：复用 failure_resume.rs 的 OccupiedBoundaryContext 与 read-first 模型，组合 F10+F12（即 scenario C7）：旧 task GC 占道 → 新 task anchor debt/read-first → 旧 prepare 在途 → Provider 失败 → 释放两门 → 新失败 debt 被真正捕获；不取消应 CheckpointDurable 后 TurnFailed 且无多余 RecoveryRequired；取消只一个 TurnCancelled 不发布迟到 TurnFailed；再次恢复同 TaskId 指令身份与工具结果不重放。
- 停止：不绕开 durability gate；继续复用现有单通道，不新增调度器。

### A-2 — 收敛诊断与最后文本轮（BR3+BR4，P2，agent-runtime）——已关闭（`0c643a2b`）
- BR3 现状：`coverage_saturated`（新窗口不在已知集合且覆盖表已满）被并入 `equivalent_observation` → 同版本、current 的未见窗口返回 `Repeated`，下游停滞/前沿提示把首次阅读新区域说成重复已知内容。修复：容量用尽返回不可比较/未追踪（不伪造 covered、不造重复证据）；先尝试不扩大集合的合法区间合并；窗口摘要不直接驱动强制完成。回归用实际生产 cap：新窗口、已知窗口、可桥接窗口、新 revision 的分类与下一次 prompt 核对；不提额、不删正文。
- BR4 现状：`force_budget_finalization` 清空工具 schema/mandatory 表示最后一轮纯文本，但更早算出的 `unavailable_must` 拒绝分支只豁免 `completion_repair_terminal`，不豁免预算文本收尾——不准备再调用工具仍可能因缺失工具被拒发最后一轮。修复：统一显式 text-only 收尾模式，由模式决定该轮是否还有工具执行义务；普通执行轮 MustSurface 仍严守；文本收尾可解释限制但不调用工具、不加隐式额外轮次、不自动宣布任务验收。回归固定总请求上限＋缺失工具，验证可报告受限结果、Core 权限不放宽；预算强制 final 与模型自然收敛分开统计。
- 停止：未知覆盖不伪报重复；文本收尾和安全边界都通过；不引入强行自主完成的启发式。

### C-1 — 测试控制器清理、退出与产物保全（BR5+BR7，P2，scripts）——已关闭（`2bf7e1d6`）
- BR5 现状：`Popen.communicate(timeout=780)` 超时不杀子进程，finally 只关 relay 写 usage 账；metadata/summary 在 finally 之后被异常跳过；child 非零退出／`protected_unchanged=false` 只 print 不进 runner 退出码，外层编排可能误判阶段成功。修复：一处无条件收尾拥有 child/relay 结束顺序（停新请求→限时请求停止→只终止自己创建的进程树→reap→写清理结果），正常/异常/取消都写终态回执；child 非零、保护文件变化、recovery fence、账目不完整分别有明确退出状态；不杀无关进程，未确认清理独立报告；错误不伪装成应用验收失败，成功也不掩盖保护边界失败。
- BR7 现状：campaign 路径固定，`setup()` `exist_ok=True` 无条件重写种子 FILES 与 baseline-lock；`l0()` 先调 `setup()`——重跑把已改应用覆盖回 v1、新增模块残留成混合工作区、baseline 重置 PREPARED/0 calls 而旧 segment 回执仍在。修复：新 campaign 排他创建；resume 只验证已有身份不重铺 seed；显式重置单列操作（新目录或明确破坏性操作）。
- 定向验证（本地进程/fixture，不接真实供应商）：超时、KeyboardInterrupt、启动失败、挂起 relay、正常终态、第二次 setup 拒绝且原应用/baseline/回执哈希不变、child 非零、保护 hash 变化。

### C-2 — campaign 额度与 attempt 账（BR6，P2，scripts；与 C-1 同文件，C-1 后串行）——已关闭（`2bf7e1d6`）
现状：每 segment `spend=0` 不读取/继承全 campaign 硬余额；只查已结算 spend 无受理预留（0.99<1.00 仍放行估计 0.10 的请求，并发可同见旧余额）；usage parser 任一计数字段即接受、缺失 input/output/cache 随后 `or 0` 补齐；上游 open/读流的部分异常路径不进 Unknown 结算；$1.04 为 runner 自身估计非独立复算账单。修复：沿现有账目为整个 campaign 持久 committed/reserved/unknown，每个真实 attempt 先预约有限额度、结束统一结算；缺测不补零，拿不到可靠上界停止新付费受理；跨段沿剩余额度继续；usage 字段按实际端点解释（T8 走 Chat、runner 走 Responses，不因模型名共用提取函数）；预算受理校验 CLI 数值有限非负，采样率/时长/轮数等实际配置写入回执；不靠调大额度让测试完成。本地 relay 用合成 usage 测试：完整/部分/无 usage、read 失败、429 重试、并发、跨段重启、临界末次调用。合成用量不得写成真实费用。

### E-1 — 证据分层与 T8 标签修正（文档）——已关闭（标签修正 `b78a3c94`；证据包与 F 矩阵映射随批收口提交）
分层记录六项真实状态：自主交付／人工修复后验收／Runtime 故障覆盖／应用负载／KV 命中／费用对照，各自关联实际证据或 NOT_EXERCISED（scenario.json 已有层次基础，不另建报告系统）。已修：t8 walkthrough 的 cross-run first-request hit 标签（首轮 1536；16896 为 11 轮段总）。**证据包已归档**：[evidence/](experiments/runtime-endurance-v1/evidence/README.md)（本机 `target/` 产物的回执级脱敏子集：总回执/final-app 验收/baseline-lock/soak/P3 回执、5 个测试控制器源码、8 个 segment 的 metadata/summary/usage 账本与纠正指令、冻结的最终 app+tests+oracle 身份；逐文件 sha256 清单＋[EVIDENCE_MAP.json](experiments/runtime-endurance-v1/evidence/EVIDENCE_MAP.json) 把 F01–F20/C1–C8 逐项链接到 campaign 证据、具名仓库回归或 NOT_EXERCISED）。已知限制如实：人工修复前的应用快照未被捕获；usage 账本为 runner 估计非供应商账单。下一份付费实验前：先定向反例与短同任务轨迹（纠正—工具执行—失败—恢复—交付），应用负载实现相关变更才重跑 soak；KV 布局对照须同任务、同起点、同验收并含缓存读写/主/维护/重试总成本；错误终结约束得到的短上下文与误判重复得到的少轮次不算优化。

## 第十一批（`d3a05d29` 续审：C0 残余＋H1–H5＋KV 扩展）——已全部关闭（2026-09-18，本地集成回归全绿；CI 终验 run `35289417269` 全绿）

审查基线 `d3a05d29`（报告：[REVIEW.md](reviews/2026-09-18-review-d3a05d29/REVIEW.md)，任务规格：[NEXT_ACTIONS.md](reviews/2026-09-18-review-d3a05d29/NEXT_ACTIONS.md)，覆盖表：[COVERAGE.md](reviews/2026-09-18-review-d3a05d29/COVERAGE.md)，CI 摘录：[CI_OBSERVATION.md](reviews/2026-09-18-review-d3a05d29/CI_OBSERVATION.md)，机制探针：[MECHANISM_RESULTS.json](reviews/2026-09-18-review-d3a05d29/MECHANISM_RESULTS.json)）。主线：**启发式相关性不能直接决定约束失效；冷热驻留位置不能决定语义；字节预算不能代替字符边界；写前拒绝不等同于日志损坏。** 审查环境无 Rust 工具链，红例全部由实施补齐并实测转绿；五个切片文件所有权互不重叠、五 agent 并行实施、按片独立提交；同一 crate 内串行（H1→H2；H3→H5）。第九/第十批修复保留未重开。

### C0 残余 — t7 旅程等待的事件链最小诊断（agent-host tests）——已关闭（`7b399497`）
回执：[C0_RESIDUAL_JOURNEY_DIAGNOSTICS_RECEIPT](reviews/2026-09-18-review-d3a05d29/C0_RESIDUAL_JOURNEY_DIAGNOSTICS_RECEIPT.md)。根因上游收口（`b06892de` 钉宽 120s＋attempt 2 绿，见 CURRENT.md 第十批终验）；本片在其上加 `JourneyTrace` 事件链诊断层（脚本模型/wire 审批/Runtime 事件/wire 里程碑/wait 失败磁盘证据五路事实，成功路径零输出），120s 数值未动、磁盘断言保留。两种人为注入（内容不符/文件缺失）验证报文可区分后完全还原；journey 本机 9.4–9.5s 连绿、host 全套 0 失败。限制：事件记录器最佳努力（Lagged 如实记）；审批未出现场景未注入验证。

### H1/H2 — Context 语义生命周期（B，context-simple；H1 先行）——已关闭（`d4a0bcbe`）
回执：[H1_H2_CONTEXT_LIFECYCLE_RECEIPT](reviews/2026-09-18-review-d3a05d29/H1_H2_CONTEXT_LIFECYCLE_RECEIPT.md)。H1：`has_retention_protection` 分词删除内部撇号（`'`/`’`→`dont`）＋补常见缩写否定词；裸 `no` 有意不加；「Remove X」正对照保持生效。H2：持久化有界 `PendingColdSemanticIntent` 环（Supersede/Verify，cap 64＋溢出计数，serde 持久化）镜像 `PendingColdConsumed` 先例；记录点 UserMessage/verify.run 成功臂；应用点三个安装路径（批量排空、按 id 服务、**restore 重水化批**）；匹配规则与已加载扫描同源（`entities_match_exact`＋`names_the_same_requirement_in`、Verify 过 `has_matching_verification_evidence`），终态经 `apply_terminal_semantic` 落 `pending_ingest_transitions`。红→绿全部实测：五位置等价（heap/warm/retry/已加载 external/未加载冷卡）、restore 不复活、跨任务隔离、瞬时读失败意图保留、冷卡 Error 经匹配探针 VerifiedFixed。context-simple **458/0**（基线 448＋10）。限制：意图环有界窗口非全历史义务；content 副本截断 4000 chars；中文缩写语义不在本片。
- 原始要点：H1（P1）——`don't`/`don’t` 绕过否定保护，`has_whole_entity_cue` 把否定句当整体撤销，仍有效的旧决策被排入 Superseded；H2（P2）——supersession/verified 扫描不覆盖未加载 `pending_external_cards`，安装按卡片旧状态装回 Live 复活（与 H1 反向）。

### H3/H5 — 真实源结束与真实文本交付（A/B，tool-runtime；两片分开成型）——已关闭（H3 `766b4136`、H5 `56312531`）
回执：[H3_H5_RETRIEVAL_AND_DELIVERY_RECEIPT](reviews/2026-09-18-review-d3a05d29/H3_H5_RETRIEVAL_AND_DELIVERY_RECEIPT.md)。H3：预算耗尽时有界 1 字节 EOF 探测（take 内层句柄原地读）区分 SourceEnd/BudgetStop，`ScannedPosition.complete` 变诚实，footer/has_more/游标全由它派生，F1/F2/G2/G3 语义未动；cap 恰好边界（含共享捕获器产出的 8 MiB）有限终止，cap+1 仍如实不完整。H5：stdout/stderr 各自独立 `Utf8Tail` 增量解码（valid_up_to/error_len 语义、≤3 字节尾缀、EOF lossy 冲刷），原始工件逐字节不变、字节统计/背压/取消不变；`OutputChunk.eof` 结束标记调用点零改动。红→绿实测：修复前 cap 走查 900 页不收敛（红跑 407s）、跨片字符产出 U+FFFD；修复后 tool-runtime **308/0/1**（基线 299＋9，耗时与基线持平）。限制：未测量 release 性能不报收益；超预算工件每页重扫为既有 W06 语义未扩权；sealed 工件 digest 复验成本属既有完整性设计。

### H4 — 拒绝阶段决定恢复许可（C，agent-storage）——已关闭（`ea5cbe76`）
回执：[H4_WAL_REJECTION_STAGES_RECEIPT](reviews/2026-09-18-review-d3a05d29/H4_WAL_REJECTION_STAGES_RECEIPT.md)。`AppendFailure` 按阶段分类：写前拒绝（容量/帧）writer 保持健康＋`append_and_sync` 内恰好一次压缩重试（压缩后仍容不下显式拒绝、不循环）；seek/write/flush/sync 失败维持 sticky fence 一字不放宽。G4 生命周期锁、WAL 格式、生产门限未动；测试走 `cfg(test)` 成对 seam 未改生产门限。5 条新测试：拒绝前后 WAL 逐字节不变＋marker 有效＋同句柄可 compact 可再 append；压缩一次后有界重试落盘；压缩后仍拒绝保持健康；撕尾与 sync 故障负对照仍封禁。红→绿：临时还原「任何 Err 一律 fence」旧语义 3 容量测试转红（矛盾形态报文）后恢复全绿。agent-storage **35/0**。限制：`compact_locked` 自身失败原样上抛；空日志拒绝的 no-op 压缩未跳过；agent-core 对任何 append 错误仍 latch recovery（上层自动 compact 属后续决策）。

### KV — 有值缓存桶、实际维护、失败/重试轨迹（C，agent-compose tests）——已关闭（`a31e9434`）
回执：[KV_VALUE_LANES_RECEIPT](reviews/2026-09-18-review-d3a05d29/KV_VALUE_LANES_RECEIPT.md)。现有 26 轮轨迹一字未改（纯增量 +1213），新增三测试：**有值桶**（4 轮 LOCAL SYNTHETIC cached/write/miss 组合，缺测保持 Unknown 不补零、typed/flatten/总额三方一致、hit+miss==input 可复算）；**失败/重试**（生产 `RetryingTransport` 下 `response.failed` 带已知 usage→重试成功：3 次线上请求 2 轮账、失败成本不抹不重复计、真实 `ModelRetrying(attempt=2)`、attempts/retries 精确）；**实际维护 lane**（生产 `ModelBackedCompactor`＋回合内 `spawn_maintenance` 路径真实触发 3 次折叠：维护 lane key/无 tools/压缩 prompt/explicit cache options，`ContextCompacted` 逐字结算，主 lane 严格分离无混账）。kv_production_sequence **4/0**（14.1s，3 次无 flake）。诚实边界：全部 LOCAL SYNTHETIC，ENDPOINT_ACCEPTED/SERVER_HIT/NET_TASK_COST 仍 NOT_RUN 归 T8。

### 第十一批集成回归（2026-09-18，本地 Windows，全部实际执行）

`cargo test -p context-simple` 458/0；`-p tool-runtime` 308/0（1 ignored）；`-p agent-storage` 35/0；`-p agent-compose` 全套 0 失败（含 proof_supervision 与 KV 14.04s）；`-p agent-conformance` 0 失败；`-p agent-workspace` 0 失败；`-p agent-host` 全套 0 失败。`cargo fmt --all -- --check` 干净；`cargo clippy -p context-simple -p tool-runtime -p agent-storage -p agent-host -p agent-compose --all-targets -- -D warnings` 干净。未执行：真实供应商实验（T8）、Unix 平台语义、GUI。CI 终验 run `35289417269` 七 job 首跑全绿（Windows full 22m12s）。




## S1 — 冷目录测试自锁（已关闭 2026-09-15）

`externalize_growth_demotes…` 曾在持有 state guard 时调用 `fetch_external`（内部重取同一锁），确定性自锁（CI run `34917761534` 两 job 取消）。修复：锁内只快照、guard 释放后经公开 API fetch、重锁核对；owner 断言改为集合比较（旧∪新 vs 热∪pending，不重叠，替代单纯总数）。cold_bounds 2/2、B2 3/3 绿、clippy 0。

## 第二批（S2 可并行修正）

### S2a — 最终装箱身份/覆盖一致性（B/C，agent-runtime）——已关闭（2026-09-15，`886e1d7f`）
required_item_ids 的清理在物理记录被移除时无条件执行（旧代码仅在记录 miss 时清理——异 ID 覆盖时 ID 残留导致 validate fence）。回归 `covered_required_id_leaves_required_item_ids_without_structural_failure` 红→绿。
`record_final_pack_drop` 允许异 ID 同路径/版本/范围覆盖被删正文且不记 miss；但被删 ID 仍留在 `required_item_ids`，最终 validate 报其缺失并中止——证据仍在帧内却结构性失败。修复：分开"物理记录是否仍存在"与"证据义务是否被覆盖"；由覆盖关系决定 miss 与义务承担者，不由"是否刚添加 miss"顺带决定身份清单。回归须走最终裁剪→覆盖计算→最终校验（含：同 ID 双层删一份、异 ID 覆盖、异版本不覆盖、真预算不足如实 miss）。不要清空 required IDs 或放宽 validator。

### S2b — 官方缓存 mapper 协议一致性（C，provider-openai）——已关闭（2026-09-15，`ea9c0e5c`）
declared 断点路径重写 string content 为 content-block 数组（`input_text` 块携带断点），与 legacy 单断点分支和官方文档一致；`function_call_output` 项的 `output` 同样包装为 content-block 数组并携带断点。`endpoint_shape_tests` 从差异 pin 翻转为正确形状断言。compose wire 验收 B0 断言更新为 ContentPart 形式。
declared 多断点仍放 input item sibling；旧单断点分支反而已是 content-block 形。按官方 Responses 文档（断点在受支持 content block 上）修正真实 mapper，fixture 断言正确形状；`endpoint_shape_tests` 现有的"差异 pin"测试翻转为正确形状断言。兼容网关方言（若确认）分开命名；未知能力端点继续不发送专属字段。这是本地可完成的协议修正，不等 T8 付费实验。

## 第三批（S3 主体能力）

### S3 — 固定预算冷目录闭环（B，context-simple）——已关闭（2026-09-15）
T4 一期/二期已有预算与降级，但"有界"仍是局部控制（续审 R3/R4）：(a) deadline 只在批间检查，单次读取不被剩余期限约束；(b) 批 take 按条数不按剩余字节预留；(c) per-id fetch/inspect 绕过热上限可无限扩大热表；(d) demote 跳过 pinned/无 claim，无可降级项时不报告背压；(e) 非空搜索命中丢掉 HydrationOutcome——1 命中/limit 20/大量未读页时模型看到普通结果无覆盖缺口。收口方向：统一安装/读后驻留/降级/预算结算到同一 metadata-residency 入口；搜索结果沿引擎→服务→Core→模型正文传 hits+coverage+remaining+stop_reason（复用 HydrationOutcome）；固定预算下连续访问不同冷页仍可取正文且热资源受控或明确背压；续查真正推进到后续冷页（不反复撞同一满员热表）。命名收口：`hydrate_all_pending_cards` 已不保证读完全部 pending——改名或契约化（不追加历史补丁注释）。
落地回执：[S3_FIXED_BUDGET_COLD_DIR_RECEIPT](reviews/2026-09-15-continuation-review-258eb4eb/S3_FIXED_BUDGET_COLD_DIR_RECEIPT.md)。单次读取进剩余 deadline（超时保持 pending owner、`HydrationStop::Deadline`）；安装前按估算字节预留、装不下保持可寻址 pending；`ExternalMap::stamp_access` 保 claim（根因：get_mut 访问戳杀 claim 使读过条目不可降级）+ 统一 `settle_metadata_residency` + `demote_overflow` 类型化 `DemoteOutcome`/`hot_metadata_backpressure`；搜索沿引擎→Core→模型正文传 coverage+continuation（坏页不挡后续页、续查累积 skip 收敛）；`hydrate_all_pending_cards` → `hydrate_pending_cards_within_budget`。context-simple 414/0、clippy 0、7 条新反例红→绿。限制（wire 层 coverage、续查 token 不进 checkpoint、pending 目录仍随历史增长）见回执。

### S4 — T7 补进程边界与指令传递证据（A，agent-host）——已关闭（2026-09-15）
现旅程的同进程重组保留，但补两个证据：(a) 独立进程变体——用已有 host 二进制新 OS 进程（不同 PID）、确认前一进程退出、从磁盘恢复同 TaskId/lineage 继续；(b) 纠正传递证据——唯一标记的 steer 指令，脚本 provider 检查实际收到的请求确实包含该标记与任务约束（缺失即拒绝推进），恢复后的请求也断言包含正确剩余义务。不换真实付费模型。
落地回执：[S4_PROCESS_AND_STEER_EVIDENCE_RECEIPT](reviews/2026-09-15-continuation-review-258eb4eb/S4_PROCESS_AND_STEER_EVIDENCE_RECEIPT.md)。新测试 `host_process_variant`（named pipe/UDS 双入口）：真实 host 二进制两进程、`pid confirmed gone` 后新进程接管 stale host.lock、`work/restore` 携带被杀进程 RunId、同 TaskId 重聚焦且无效果重放；脚本 provider 纯函数门禁（缺目标/缺标记/恢复后缺剩余义务即拒绝），写入内容由请求体内实际送达的 token 派生。host 套件 31/0、clippy 0、进程变体 3 次重复全绿。限制（同进程旅程保留、watchdog 全路径不声称、unix 入口未本机运行）见回执。

## 第四批（V 系列四切片；V1 先行，其余可并行）

审查基线 `4f6eb7ff`（报告：[REVIEW.md](reviews/2026-09-16-review-4f6eb7ff/REVIEW.md)，行动与停止条件：[NEXT_ACTIONS.md](reviews/2026-09-16-review-4f6eb7ff/NEXT_ACTIONS.md)）。共同主线：分页只改变驻留位置，不改变记录的语义身份与保护义务；取消只改变执行结果，不抹掉已知成本。共享 ContextEngine/service wire/ModelRequest 契约由单一集成人维护；行为修改与机械移动分开提交；定向测试后跑既有相关跨 crate 集成，合并沿用现有 CI。

### W1 — 逻辑 owner 与冷页解析（B，context-simple；含 V1＋V2）——已关闭（2026-09-16，`c0923af2`）
用户动作：历史已外置/分页后，Agent 仍可正确要求这份证据进入下一次请求；运行 GC 不会让未读卡片失去有效 scope。
- V1（P1）：`retire_closed_scopes` 的 referenced 集合只枚举 heap/Warm/写入重试表/已加载 external/active scope，未加载 pending 卡片中的 scope 引用不可见——退休可移除仍被未读冷页引用的 scope，`hydrate_card_for` 读回时因 scope 不存在而消费定位行，破坏可达性/恢复目录。修复：把元数据/引用闭包完整性传入退休许可——未证明闭包完整时不退休未知冷页可能引用的节点；不依赖完整引用信息的内存 GC 照常。不许删 scope 校验放行坏结构；不许全历史加载替代分页。长期可沿目录维护 scope 引用计数（须与分页 owner/迁移/checkpoint 原子一致，不建第二份任务真相）。
- V2：`materialize`→`plan_required` 只查已加载索引，pending 卡片不在解析路径——同一正文 `fetch_external(id)` 可读、声明成 PromptRequired 却报 Missing。修复：必需正文规划前对有界 required refs 做目标解析（精确 ID 直接定位卡片；路径/实体沿冷目录）；区分 不存在/读取失败/策略排除/因预算未解析，不压成 Missing，不全历史 hydration。
- 回归：卡片保持未加载→GC 触发退休→按 ID 读取→checkpoint/restore，核对正文、scope 关系、owner 集合（证明目标卡片本轮未加载，不得靠 fixture 顺序巧合进热表绕过反例）；required 目标位于 restore 首批之后＋固定热预算＋精确 URI 直接 materialize。
- 停止：同一正文在已加载与分页状态下语义保证一致；正常退休仍可收敛（释放最后一个真实引用后才退休）。
落地回执：[W1_OWNER_AND_REQUIRED_RESOLUTION_RECEIPT](reviews/2026-09-16-review-4f6eb7ff/W1_OWNER_AND_REQUIRED_RESOLUTION_RECEIPT.md)。闭包不完整时有界探测收集 pending 卡片 scope 引用、预算耗尽诚实推迟退休（`scope_retirement_deferred`）；required 解析走 per-id lane＋有界目录扫描，原因类型化 Missing/Corrupt/IoFailed/UnreadColdPage。context-simple 425/0，4 反例红→绿。限制：pending 超探测预算时退休持续推迟（诚实保守）；foreground optional_misses 未做未读区分。

### W2 — 原子搜索结果与 continuation 生命周期（B/C；含 V3＋V4＋V5）——已关闭（2026-09-16，`c0923af2`）
用户动作：部分命中时 Agent 知道尚有未读区间并能在固定预算内续查；恢复/目录变更后旧游标不能错用。
- V3：`ContextServiceAdapter::search_external` 只返回 `Vec<ExternalizedContext>`，未覆盖 `last_search_coverage`/`search_external_continuation`，继承默认 complete＋忽略 token——service 模式退回「普通完整结果」。修复：一次搜索原子返回 hits+coverage+observation+continuation/视图身份（不靠调用后再读可变 `last_search_*` 旁路）；旧服务无该能力时明确 Unknown/Unsupported，不默认 complete；经既有协议版本/能力协商迁移。
- V4：`record_search_coverage` 在 query_key 相同时无去重追加旧 `covered_ids`，普通搜索不持 token 也可持续追加同一热窗口——固定历史下重复普通查询即可让 retained state 随调用次数增长。修复：fresh（开始新遍历，不继承）与 resume（验证过的遍历）分开；覆盖状态去重、链级条数/字节边界；超界返回显式状态。
- V5：`restore` 替换 State 但不清理 State 之外的续查状态；token 编号由 slot 尾数推导、链完成后清空重开——旧 token 可在原位 restore 后或新链 ABA 命中。修复：成功 restore 使旧 token 失效（拒绝 restore 不动现有合法状态）；token 用进程生命周期单调 nonce/不透明身份并绑定目录/恢复代际。
- 回归：固定历史上重复普通搜索，retained state 不随调用次数增长；固定预算续查每次推进不漏页；同一冷热 fixture 下 in-process 与 service 模式的 非空不完整/空不完整/续查到末页/过期 token 语义一致；视图 V1 签发 token→恢复 V0→旧 token 不得把 V0 未搜索内容当作已覆盖。
- 停止：一次查询结果自带 coverage；fresh/resume/restore 一套生命周期规则。
落地回执：[W2_ATOMIC_SEARCH_AND_CONTINUATION_RECEIPT](reviews/2026-09-16-review-4f6eb7ff/W2_ATOMIC_SEARCH_AND_CONTINUATION_RECEIPT.md)。`ContextSearchResult{hits,coverage,observation}` 原子返回＋新 wire op `SearchExternalReport`（握手协商 `context-search-report.v1`，未协商显式 Unsupported）；fresh 不继承、resume 去重＋链级 16384 条上限、restore 失效全部旧 token、nonce＋恢复代际防 ABA；`last_search_*` 旁路诚实化为 Unknown 且 service/Core 均不读。context-contextcore 9/0、agent-context-service 11+20/0，红-first 7 条（变异恢复法）。限制：已消费 token 重试采稳定降级 fresh；covered 集仍是累积 ID 集非目录快照游标（审查长期方向）。

### W3 — 工具结果缓存断点块类型（C，provider-openai；V6）——已关闭（2026-09-16，`9b176df0`）
用户动作：显式标记可复用工具结果时，请求使用合法输入内容块，不发错误类型或未确认 fallback。
- `build_responses_wire_request` 的 function_call_output 分支把 output 包成 `output_text` 块——官方 Responses 缓存断点只支持 input_text/input_image/input_file；另有未确认的 input-item sibling fallback 残留。现有 `endpoint_shape_tests` 工具用例只断言"数组内有断点、顶层没有"，未查块类型（函数名/注释还与断言相反）。
- 修复：受支持 input 块类型化映射；无法合法承载断点时剔除 hint 并记录原因（或拒绝该缓存计划），保留真实消息与工具配对；网关方言显式独立命名。普通文本 input_text 已修部分不重做。
- 回归：fixture 断言块类型/位置/消息展开后的对应关系（不只字段存在）。本地无需付费可完成；真实接受/命中/净费用仍是 T8 条件实验，不互相替代。
落地回执：[W3_TOOL_OUTPUT_BREAKPOINT_BLOCK_RECEIPT](reviews/2026-09-16-review-4f6eb7ff/W3_TOOL_OUTPUT_BREAKPOINT_BLOCK_RECEIPT.md)。function_call_output 包 `input_text` 块（官方 multi-turn 示例同形）；未确认 sibling fallback 全删；`place_declared_breakpoint`×`BREAKPOINT_BLOCK_TYPES` 单一映射 seam，无法承载时类型化剔除＋记录。provider-openai 155/155。

### W4 — 取消不抹掉已知用量（A/C；V7）——已关闭（2026-09-16，`4fa2d8a2`）
用户动作：失败调用已报告费用→退避等待中用户取消→账目仍保留已知数值，执行状态仍是取消。
- 现状：backoff cancel 分支把 known_usage 写 CallStage 后返回普通 `AgentError::Cancelled`；生产 observer 是 `JsonlRetryObserver::from_env()`——未设 `OPENAI_RETRY_METRICS_FILE` 时 append 直接返回；Runtime 把 Cancelled 映射为不带用量的 `OperationOutcome::Cancelled`。已知信息被降级为无正式去向。
- 修复：沿既有模型 operation 结算入口把 outcome 与 usage 正交表达——取消仍保持 Cancelled 分类与安全屏障/代际隔离，已知 usage 走必有的结算通道，JSONL 只留诊断副本。不许用 `FailedWithUsage(Cancelled)` 换返回值而不调调用链（会把取消误分类为失败）。
- 回归：Compose→Retry→backoff cancel 全链、metrics 环境变量不存在：已知用量保留、取消分类不变、未执行重试不多计、未知字段不补零、同次 SSE 累计快照不重复相加。
落地回执：[W4_CANCEL_SETTLES_KNOWN_USAGE_RECEIPT](reviews/2026-09-16-review-4f6eb7ff/W4_CANCEL_SETTLES_KNOWN_USAGE_RECEIPT.md)。`FailedWithUsage{source:Cancelled}` 携带（裸 Cancelled 不变）、`OperationOutcome::Cancelled{known_usage}` 守卫臂结算、屏障前按真实计数记账＋fence、迟到 stale completion 经有界 FIFO 一次性补记；observer 降级为诊断副本。全链回归在 env 不存在下断言正式账目。限制：maintenance lane 取消仍 unknown（token 永不取消）；迟到成功不补记（既有设计）。

## 第五批（U 系列；U1 先行，A 线内部按文件所有权串行）——**实现已提交，但续审确认 U3 引入 R1 回归，A 线不算关闭**

审查基线 `d92564bc`（报告：[REVIEW.md](reviews/2026-09-16-review-d92564bc/REVIEW.md)，行动与停止条件：[NEXT_ACTIONS.md](reviews/2026-09-16-review-d92564bc/NEXT_ACTIONS.md)，覆盖表：[COVERAGE.md](reviews/2026-09-16-review-d92564bc/COVERAGE.md)）。A1–A4 的实现与提交均已完成；`3bdb269c` 续审发现其中 U3 的精确一次身份去重破坏实时分片（见第六批 R1），**保留已完成改进、不重开旧问题，但 A 线整体转入第六批收口。** 共同主线：**让 TUI 成为可信的操作入口（一套读模型、一个保序提交口、一个布局/宽度模型），而不是另一份会自行漂移的运行状态；让 required 解析产生稳定的执行计划，而不是依赖最后剩下的热目录内容。** 都属于主体完善，不重做架构、不前置 GUI。行为修改与机械移动分开提交；定向测试后跑既有相关跨 crate 集成，合并沿用现有 CI。

### A1 — 审批可核对与多行文本渲染（U1＋U2，agent-tui）——已关闭（2026-09-16，`22ab97a6`）
用户动作：待审批的长命令/长路径/大段替换内容能在确认前翻到尾部核对；代码块、错误栈、计划按原行结构显示。
- U1（P1）：审批区域固定高 3（扣边框只剩一行）却要装工具说明＋参数＋确认提示；`state::begin_approval` 总参数预览只取前 220 字符、对话参数日志最多 8 项且每项只取前 120 字符且不标记尾部截断；`session.rs` 审批中只接受允许/拒绝，PageUp 等导航被忽略。
- U2：`conversation_lines` 把整条正文交给 `Line::from(content.clone())`，Ratatui 0.30 的 Line 构造/转换会移除换行（非多行 Text 容器）；滚动按总宽度估算折行而 Paragraph 用自己的换行规则；光标用 `chars().count()` 而非终端显示列宽。
- 修复方向：保留受 Core 已有请求上限约束的完整审批数据或可信完整查看引用＋可滚动详情＋明确标记截断；确认始终绑定当前 `request_id`。多条 Line 或真多行 Text；视窗/滚动/光标共用同一布局宽度口径。不扩展主题/插件面板/GUI，不做 Markdown 编辑器。
- 红例：两条长参数前 120 字符相同、关键目标只在尾部不同 → 80×24 与窄终端经真实 render ＋翻页能核对尾部 sentinel；超 8 参数有继续查看路径；过期请求不能批准新请求（旧确认不作用于新 request_id）；多行正文的第二行、缩进、空行、末行 sentinel 出现在正确 buffer 行；中文/宽字符长输入光标落在输入区内。
- 停止：实际渲染/输入用例通过且批准/拒绝仍走同一 Core gate；不靠 CaptureSink 或对内部字符串取子集证明屏幕可见。
- 落地回执：[A1_U1_U2_APPROVAL_AND_RENDERING_RECEIPT](reviews/2026-09-16-review-d92564bc/A1_U1_U2_APPROVAL_AND_RENDERING_RECEIPT.md)。`PendingApproval` 改 `detail: Vec<String>`＋`truncated`（完整请求，无 220 字符上限）；`approval_scroll`＋`PgUp/PgDn`（`classify_approval_key` 纯函数分离回答键与导航键）；确认绑定屏上 `request_id`（过期确认不批准新请求）；对话摘要截断标 `…` 并指向面板。`conversation_lines` 按 `'\n'` 拆真 `Line`；折行/滚动/光标共用 `display_width` 显示列宽＋横向视窗。8 条新测试经真实 `ui::render`＋`TestBackend` buffer 断言，变异恢复法复验两条关键用例转红后 sha256 还原。agent-tui 75/0、clippy 0、fmt clean。限制：无真实 PTY 端到端；`display_width` 为内联宽字符表，未覆盖全部 Unicode 组合字符；未复核 Core 侧请求上限。

### A2 — 一份事件读模型与按任务 review（U3＋U4，agent-tui＋agent-runtime）——已关闭（2026-09-16，`baa2ca70`＋`7224ec6d`）
用户动作：`/status`、`/review` 与现场画面一致；重放/重复投递不改变结论；任务 B 的修改与失败不挂在任务 A 的完成头下。
- U3：`AppState` 同时维护公共 `StatusProjection` 与本地 `status/busy/current_model_operation/current_task`/局部 Token/对话/`result_card`；`resync_projection` 只重建公共投影；重放水位只跳过 `projection.fold`，同一已覆盖事件仍继续改后面本地字段；对话与部分 live 消息按**正文内容**去重；`StatusProjection` 自身未折叠 `TurnCancelled` 等终态；坏行被跳过、目录被截断仍报完整且把最大 seq 当连续水位。
- U4：`ResultCard` 的 changes/checks 无各自 TaskId，A 完成后开始 B 不会切换卡片；容量上限满后直接不追加且无独立遗漏计数，`format_result_lines` 用 `len-cap` 算溢出恒不可见；`result-card-latest.json` 由独立 task 写同一路径，无单写者/版本化原子快照。
- 修复方向：一套共享事件折叠规则，按事件与操作身份（RunId/seq、TurnId/OperationId/generation）去重，实时消费与重放同一规则；输入草稿/滚动位置等纯界面状态另留 ViewState；按任务归属或查询复核材料，显示窗口/总数/遗漏数分离，晚到失败不被容量限制静默掩盖；快照写入单写者或版本化原子提交。不新建任务真相、不从 prose 推断完成。
- 红例：实时逐条消费得状态 A，另一实例先遗漏一段再重放＋重复投递已覆盖的 **ModelUsed/TurnCompleted/操作切换**事件（不是 `RunStarted`——它只置 bool，去重失效也可能通过）后必须等价；取消无 usage 清理 in-flight；相同文字不同 turn 保留；A 完成 → B 修改并校验失败 → `/review B` 不得出现 A 的完成头；第 33 个 check 的失败不得静默消失；坏 JSON 中间行/日志覆盖不全保持 Partial。
- 停止：现场画面、`/status`、`/review` 与相同已验证事件视图一致；旧事件不能反向激活操作。
- 落地回执：[A2_U3_U4_EVENT_MODEL_AND_REVIEW_RECEIPT](reviews/2026-09-16-review-d92564bc/A2_U3_U4_EVENT_MODEL_AND_REVIEW_RECEIPT.md)。U3：`apply_runtime_event` 先 claim `(RunId, seq)`，claim 被拒即**整体返回**（不再只跳投影折叠）；折叠体抽到 `apply_event` 且**重放调用同一函数**；`resync_projection` 先归零事件派生字段（`reset_event_derived_view`，纯界面状态与追加型转录不动）再重建；转录行按事件身份去重（`claim_message_row`）＋用户气泡按 `input_id`，5 处正文比对删除；`AssistantMessage` 只终结本轮自己流式打开的行（`streaming_row_open`）；`StatusProjection` 补折叠 `TurnCancelled`；坏行/短读/序列缺口 → `view_partial`＋原因，只在可验证连续前缀设水位。U4：`ResultCard` 增 `task_id`＋单调 `revision`＋`omitted_*` 计数，切换任务**归档**旧卡（有界 8 张），`/review` 走 `review_card()`（当前任务卡，否则最近归档卡，永不混合）；容量拒绝即计数并在 review 明示；快照单写者 gate＋写临时文件 rename 提交。8 条新测试，6 处变异恢复法复验转红后 sha256 还原。agent-tui 83/0、real_binary_startup 2/0、agent-runtime `status::` 5/5、clippy 0、fmt clean。**限制**：未跑 agent-runtime 全量（他人在 `execution/`、`actor/` 有未提交改动）与 workspace 全量 CI；归档上限 8 张、不能按 TaskId 查任意历史；`view_partial` 未覆盖 live `Lagged` 缺口；快照写入失败不重试。

### A3 — 保序控制与可退出的终端（U5＋U6，agent-tui）——已关闭（2026-09-16，U5 `296ec005`／U6 `96c0e5c3`）
用户动作：终端在任何早退/异常/退出路径后都恢复；用户键入顺序就是命令提交顺序；慢磁盘操作期间仍能取消/退出。
- U5：`main.rs` 手工 `enable_raw_mode` → `EnterAlternateScreen` → `Terminal::new` → clear，raw 开启后任一 `?` 早退可绕过尾部恢复；正常退出先 `await composed.shutdown()` 才关 raw/退出 alternate/显示光标；手工 `Terminal::new` 不等于安装 panic hook。
- U6：`/focus`、`/task`、`/done`、`/continue` 各自 detached spawn，Actor 只保证"到达之后"的顺序；`/checkpoint`、`/restore`（含磁盘读）在输入循环内直接 await，占住唯一的输入/绘制循环。
- 修复方向：小型 TerminalSession guard 跟踪已启用状态＋保留原行为的 panic hook；终端恢复与 Runtime 异步清理分别完成和聚合错误；有界保序命令提交入口＋可观测回执＋任务身份校验（用 Runtime 已有带 `expected_task_id` 的接口）；慢 I/O 离开绘制循环；每帧 drain 有预算。不另建调度器（RuntimeActor 仍是唯一执行权威），不把 TUI 强制改成 IPC 客户端，不宣称能恢复 SIGKILL 后的终端。
- 红例：raw 开启后注入 alternate/创建终端失败/绘制失败/session 返 Err/Runtime shutdown 慢/panic 各路径终端均恢复；人为延迟 `/task` 发送后再 `/continue` 不得推进旧任务；恢复读取被暂停时仍能处理退出/取消；已取消回执不覆盖新代际；事件洪泛时键盘响应仍有界。
- 停止：不因前端调度重排用户动作；终端先恢复，Runtime 按既有规则完成取消与清理。
- **U5 落地回执**：[A3_A4_U5_U7_TERMINAL_AND_HEADLESS_RECEIPT](reviews/2026-09-16-review-d92564bc/A3_A4_U5_U7_TERMINAL_AND_HEADLESS_RECEIPT.md)。`TerminalGuard` 经 `TermBackend` 抽象跟踪本进程已启用状态，早退/失败先回滚部分状态再返回错误，`Drop`＋panic hook（链回原 hook）恢复；终端释放排在 `composed.shutdown()` **之前**，两类错误分别聚合。2 条可注入 backend 单测，变异恢复法复验。
- **U6 落地回执**：[A3_U6_ORDERED_COMMANDS_RECEIPT](reviews/2026-09-16-review-d92564bc/A3_U6_ORDERED_COMMANDS_RECEIPT.md)。有界命令队列（32）＋唯一 worker：任务动作与 `/checkpoint`、`/restore` 按键入顺序执行；满队/worker 消失显式报告不静默丢弃；`/continue`、`/suspend` 用 `*_expecting` 绑定操作员观察到的任务，不匹配不启动回合并点名两侧；`/done` 前置快照比对后拒绝；worker 持 `checkpoint_plane()` 使运行时捕获与原子存储写**离开绘制线程**（artifact 经类型化 `ViewFact` 回传）；每帧 drain 有预算；`/quit`、`/cancel` 不排队。4 条新测试＋既有 e2e 全链路复跑，2 处变异恢复法复验转红。agent-tui 87/0、clippy 0、fmt clean。**限制**：`/done` 非原子（`CompleteTask` 无 expecting 变体，属共享契约后续项）；身份不匹配只有文案级单测无一 e2e；只读命令之间不保证顺序；慢 I/O 只离开绘制循环未离开运行时；未跑 workspace 全量 CI。

### A4 — headless 正面终态与完整性（U7，agent-tui）——已关闭（2026-09-16，`296ec005`）
用户动作：headless 只有拿到相关任务/回合的正面终态证据才报成功；事件有缺口时明确报告不完整。
- U7：`run_headless` 收到 broadcast `Lagged` 只写 warning 继续，缺口不进 `Drain`/`session_end`；丢掉 ApprovalDenied/Failure/结算后若尾随 `TurnCompleted` 仍可能 exit 0；`Closed` 直接 break 而 `Drain::finish` 兜底不要求确认 `turn_completed`（**正常 `RuntimeHandle` 自持 broadcast Sender，此项是防御性接口边界，不是正常 actor 退出必然可达的生产故障**）；timeout/输出失败后才等待 sink 收尾再 shutdown，取消启动过晚；返回的 writer 又被同步 flush 一次，不受该 writer 线程 close bound 约束。
- 修复方向：成功需要对应任务/回合的正面终态证据；有缺口先从可信水位补齐，补不齐返回明确 incomplete；停止工作与输出收尾分离；修 JSONL 双换行；保持现有出口含义或对新 outcome 做明确版本兼容。不把"不完整"解释成审批拒绝，不给缺测费用补零。
- 红例：被丢区间含拒绝事件而尾部有 `TurnCompleted` → 不得按完整成功；无终态的独立 `Closed` receiver 不得成功（标注为防御接口用例）；慢 writer 下取消不等待输出 flush；执行状态与最终 JSONL/进程 exit 一致。
- 停止：事件流、`session_end`、exit 与实际结算/完整性一致；未知不冒充拒绝、失败或零费用。
- 落地回执：[A3_A4_U5_U7_TERMINAL_AND_HEADLESS_RECEIPT](reviews/2026-09-16-review-d92564bc/A3_A4_U5_U7_TERMINAL_AND_HEADLESS_RECEIPT.md)。`Lagged` 记 `events_dropped`/`dropped_count`，终态前关闭记 `closed_without_completion`，两者任一命中新守卫臂 → 新 `EXIT_INCOMPLETE = 4` / `status: "incomplete"`（`stop` = `events_dropped`｜`stream_closed`）；尾部 `TurnCompleted` 不再掩盖被丢段；不补零、不把缺口当拒绝；`Closed` 限定为防御接口边界。另修 JSONL 双换行、非终态先取消在途回合再等输出排空、返回 writer 不再超界 re-flush。4 条新测试，变异恢复法复验两条转红后 sha256 还原。限制：`EXIT_INCOMPLETE = 4` 是新增出口码，脚本调用方需知悉；真实 PTY 端到端未执行；U3（TUI 侧事件缺口）不在本片。

### B1 — 多 required 的有界解析计划（B，context-simple）——已关闭（2026-09-17，`de6bf061`）
用户动作：多个必需正文依次从冷目录加载时，先加载的目标不因随后驱逐又被报成 `Missing`。
- `resolve_required_cold_refs` 每个 exact ID 调 `hydrate_card_for_outcome`，装完立即 `settle_metadata_residency`，protect 只含刚装的这一个 ID；解析只保存 `Installed/AlreadyOwned`，不持已验证 owner 快照；整批结束才 `plan_required_with_resolution` 按热表查找。热容量 2、required A/B/C、总量在模型预算内时，C 的安装把 A 降回 pending，规划 A 找不到 owner，`Installed` 的兜底 miss 又映射为 `Missing`。
- 修复方向：解析时直接产生**有界的、版本/范围绑定的 `RequiredPlanSource`**（或明确受预算约束的短期租赁），不依赖整批结束时谁还恰好驻留。临时计划计入资源预算；预算不足用准确的 `BudgetExcluded/UnreadColdPage`，不宣称 `Missing`；不无限 pin、不全历史 hydration。
- 红例：hot cap=2 ＋ required A/B/C 三个合法可降级冷页 ＋ 模型 budget 足够 → 读取均成功且 A 不因随后驱逐成为 Missing；再覆盖 entity/foreground 干扰与真实预算不足；混合 exact ID 与路径。
- 停止：材料化与最终装箱要么给出正确正文，要么报告真正的容量/读取原因。这是对 W1 的批量补齐，不是重开"完全没有冷解析"。
落地回执：[B1_B2_THIRD_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/B1_B2_THIRD_BATCH_RECEIPT.md)。per-id lane 改返回 `PendingIdRead{outcome, entry}`，`RequiredColdResolution::resolved` 在读取点捕获版本/范围绑定的卡片条目（每 id 首捕获生效、上限=观察上限）；规划对 exact ID、实体、前景三处加捕获 fallback（同一 `plan_store_required`/谓词门），热表查找仍优先。驻留结算不变；预算不足仍由 `apply_required` 报 `BudgetExcluded`。3 条红线红→绿＋三处 fallback 变异复验承重。限制（wrapper 的有界克隆、捕获上限、实体精确等值口径）见回执。

### B2 — 捕获完整性证明与取消安全（B2＋B3，context-simple）——已关闭（B3 2026-09-16 `3fe396c9`；B2 2026-09-17 `2482f3ef`）
用户动作：同名坏卡片存在时不把唯一可靠元数据换成坏引用；导出过程中取消不丢内存日志。
- B2：`run_external_spill_io` 的 plan.writes 分支遇到已有路径，`try_exists` 后直接放进 `io.written/io.spilled`，既未比对现有内容与计划 bytes/hash/身份，也未走受检卡片读取；`checkpoint` 随后 `record_card` 并从 inline external 段排除它们 → 同名文件损坏/截断时新 checkpoint 只引用坏卡片，下次 restore 才发现。
- B3（后续收口）：`export_ledger` 先 `mem::take(state.ledger)` 再 await 写临时文件与 rename；普通 I/O 错误有 merge back，但 future 在 await 中被取消/丢弃时不会运行该错误分支，记录随局部变量消失。
- 修复方向：首次认领未验证 existing card 时做有界校验（identity/schema/hash 与计划内容一致），或按内容寻址规则安全原子写入；无法证明时本次保留 inline。导出改为成功提交后再确认消费相应记录。不引入新存储层，不做无差别全量重读，ledger 子项可晚于恢复卡片完整性。
- 红例：无有效 claim 的 fixture 中预置同名坏文件 → capture → 新引擎 restore，原元数据仍可恢复或 capture 明确保留 inline；覆盖 existing directory／hash 不匹配／读取权限故障；在 write/rename 边界暂停并取消导出后记录仍有归属。
- 停止：证据不因同名文件损坏而失去唯一可靠副本；已有有效不可变 claim 的复用性能不被全量重读破坏。
B2 落地回执：[B1_B2_THIRD_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/B1_B2_THIRD_BATCH_RECEIPT.md)。已存在卡片路径只在「是文件、长度与计划字节相等、整读等值」时认领（内容寻址幂等免重写不变）；可读不一致/不可读落入同一条原子写入（修复或首次写入），写失败保持 inline——manifest 不再收坏引用。`recorded` 快路径零 I/O 不变。2 条红线（坏字节、目录占位）红→绿＋退回裸 `try_exists` 的变异复验双红。限制（权限故障不做专用反例、整读比较受卡字节预算约束）见回执。

### C（续）— 实际请求序列的 KV 与成本比较
不重开 W3 已修的内容块类型任务。沿实际 request 序列核对连续请求首差异、稳定证据范围与工具 schema 变化、失败/取消的已知用量只结算一次；TUI 的 Token 显示取自同一份结算事实（与 A2 的读模型一致），不因重放再算一份不同的账。质量、指令、证据新鲜度、权限撤销不得因缓存倒退。真实端点接受/命中/净成本仍是 T8 条件实验，无预算无凭据保持 NOT_RUN。

## 第六批（R 系列：`3bdb269c` 续审；R1 先修，其余按文件所有权并行）

审查基线 `3bdb269c`（报告：[REVIEW.md](reviews/2026-09-16-review-3bdb269c/REVIEW.md)，行动与停止条件：[NEXT_ACTIONS.md](reviews/2026-09-16-review-3bdb269c/NEXT_ACTIONS.md)，覆盖表：[COVERAGE.md](reviews/2026-09-16-review-3bdb269c/COVERAGE.md)）。共同主线：**身份的作用域与事实的用途必须分开**——日志游标不是分片身份，已知费用不是当前操作终态，单任务修订号不是全局发布序号。R 编号仅定位本报告，沿用三线，不建新阶段。开工先声明每个切片的用户动作、最小反例与预期事件身份；**不得通过给每个事件随机新 RunId、删除反例或放宽固定时限来"修绿"**。

### R1 — 实时分片不能被当成重复的持久事件（P1，agent-tui；与 R7 同片）——已关闭（2026-09-16，`bb761547`）
用户动作：同一会话能看到流式回答与重试进度；日志补齐后对话不重复、不消失。
- 生产者形状（已核实）：`sink.rs::LiveSink::new(core.event_sender(), core.event_sequence(), …)` 把 `ModelStarted` 的持久游标复用为每个 `ModelDelta`／`ModelRetrying` 的 `seq`；它们不写 WAL、不申请新序号。`q3bdb269c` 的 `claim_event(RunId, seq)` 因此在最外层把正常流式分片全部丢弃。
- 修复方向：持久事件按 `(RunId, journal_seq)` 去重；**实时分片按 `(TurnId, OperationId, generation)` 校验归属**（既有 `current_op` 围栏），不进入持久身份集合，也**不受重放水位过滤**。不把分片写入 WAL、不伪造持久序号。
- 停止：真实生产者形状（固定 RunId、分片复用 `ModelStarted` 的 seq）回归通过；旧 generation 分片仍被拒；重复持久 `ModelUsed`/`TurnCompleted` 仍只计一次；既有 TUI 测试通过。
- 落地回执：[R1_R5_FIRST_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R1_R5_FIRST_BATCH_RECEIPT.md)。**关键发现：契约里早已有 `RuntimeEvent::is_live_only()`**（文档写明"分片复用持久游标、投递游标不得过滤"，`agent-host` 已在用），TUI 只是没采用。已改为 live-only 事件同时跳过持久 claim 与重放水位，归属交给 `current_op` 围栏。**根因之二：折叠 fixture 每次新建 RunId 却固定 seq=1，恰好绕过消费者实际应用的身份**——已改为稳定 RunId＋递增序号。agent-tui 91/0、clippy 0、fmt clean；变异恢复法复验去掉门控后助手正文为空。

### R7 — 重放幂等与附属索引有界（P2）——已关闭（2026-09-16，R4 收口发布副作用；`ef8a835c`＋`1a3ec323` 收口其余）
- 落地回执：[R6_R7_R8_B3_SECOND_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R6_R7_R8_B3_SECOND_BATCH_RECEIPT.md)。事件派生行带事件身份，重放先丢弃再重建（会话本地行存活）；`shown_input_ids` 改为有界 FIFO＋索引。`replaying_the_same_journal_twice_yields_the_same_transcript` 钉住幂等。
- 现状：重放保留 `messages`/`shown_message_index`/`shown_input_ids`，重置运行投影后重新应用日志；User/Assistant/Tool 行有身份去重，但 **Warning/Focus/ModelUsed 等生成的 SYSTEM 行没有同等规则**，重放会再次追加并把已保留的助手行挤出 400 行窗口 → **同一日志重放两次可见序列不一致**。`shown_input_ids` 只插入不淘汰。
- 修复方向：所有事件派生行共享事件身份；更稳妥的是**构建新的有界读模型后原子替换**事件派生部分，草稿/滚动/当前审批等本地状态单独保留；`InputId` 索引跟随可见窗口与活动排队输入。**reducer 输出与写盘副作用分开**：重放历史 `TaskCompleted` 不等价于重新发起一批快照写入。
- 停止：同一日志连续重放两次事件派生视图一致；相同正文不同事件都保留；固定窗口下大量不同输入不使附属集合线性增长；不清空输入框或当前审批来简化恢复。

### R2 — 取消后的补账覆盖全部迟到终态（P2，agent-runtime）——已关闭（2026-09-16，`8877a4da`）
- 落地回执：[R1_R5_FIRST_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R1_R5_FIRST_BATCH_RECEIPT.md)。用法提取改为与业务结果分支无关的 `outcome_reported_usage`（覆盖 `ModelOutput`／`Failed`／`Cancelled`），并把「写了 Unknown 占位（accounted 未 settled，仍允许恰好一次补账）」与「已知值已入账（accounted＋settled，不可再加）」分开；`mark_usage_settled`／`usage_settled` 语义改名，**复用既有队列字段因此未动并行会话的 `actor/mod.rs`**。新回归挂起 provider 调用、等取消处理后再放行：两种迟到终态各断言业务保持取消、无工具执行、真实计数入账一次；既有 W4 回归未退化。
- 现状：取消屏障先写 Unknown 用量并把 OperationId 标记 accounted；迟到结果的补充路径**只接纳 `Cancelled { known_usage: Some(...) }`**，迟到的 `ModelOutput { usage }` 与 `Failed { usage }` 走不到 → 业务结果正确作废，已知费用一起被跳过（**W4 原形状保留，补的是终态矩阵残余**）。
- 修复方向：把用量提取从业务结果分支中收敛出来，覆盖 `ModelOutput`／`Failed`／`Cancelled`；把"已有 Unknown 占位"与"可见证据已全部结算"分开表达；沿既有 operation/accounting 状态做一次性幂等补账。
- 验收矩阵：迟到 `ModelOutput`（含 usage/工具调用）→ 不采纳正文、不执行过期工具、已知计数结算一次；迟到 `Failed{usage}` → 不重启旧操作、计数结算一次；`Cancelled{known_usage}` → 保持取消且保留现有补账；任意类型无 usage → 业务原样、Unknown 保持 Unknown；同一 completion 重复到达 → 不再执行、不再加账。**在 `OPENAI_RETRY_METRICS_FILE` 不存在时跑。**
- 停止：受控 provider ＋真实 Runtime/事件输出通过；真实 vendor 净成本仍留 T8。

### R3 — 用量事实不得清掉当前操作状态（P2，与 R2 同片）——已关闭（2026-09-16，`8877a4da`）
- 落地回执：[R1_R5_FIRST_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R1_R5_FIRST_BATCH_RECEIPT.md)。`StatusProjection::fold(ModelUsed)` 不再清 `in_flight`；用量只推进账目，活动状态由能命名终止对象的生命周期事件推进。**取舍**：模型轮结束后到下一个生命周期事件之间 `/status` 会仍显示 `in_flight=model round`（无法证明归属就不改活动状态）。
- 现状：`StatusProjection::fold(ModelUsed)` 无条件 `in_flight = None`；Runtime 允许旧调用迟到用量进入当前流 → A 取消、B 运行中、A 的迟到用量到达时，**投影显示"没有正在执行的操作"而 B 仍在运行**（是投影不准，不是 Runtime 停了 B）。
- 修复方向：**用量事实只负责账目；活动状态由能绑定当前操作的生命周期事实推进**；需要从用量关联终态时携带明确 `OperationId`/角色/代际并核对；无身份的旧格式用量只计费、不清当前操作。不得用丢弃迟到 `ModelUsed` 来"修好界面"。
- 停止：A/B 交错时 A 的补账入账一次且不改写 B 的运行显示；主调用与维护调用分开；实时消费与日志回放结果一致。

### R4 — 卡片内容修订号与全局发布序号分开（P2，agent-tui）——已关闭（2026-09-16，`d30f2956`）
- 落地回执：[R1_R5_FIRST_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R1_R5_FIRST_BATCH_RECEIPT.md)。新增会话单调 `card_publish_seq`（切任务/投影重建不重置）作为写入器排序依据，artifact 增 `publish_seq`；卡片自身 `revision` 语义不变。顺带收口 R7 的发布副作用：重放期间 `replaying` 使历史 `TaskCompleted` 不再逐个发布快照。
- 现状：`begin_card_for_task` 切任务时新卡 `revision` 从 0 起，而 `card_snapshot_gate.last_written_revision` 是跨任务共用的全局水位 → **A 完成后 B 的快照因 `1 <= 1` 被拒**，重启后 latest 仍是 A（Runtime 的 `TaskCompleted`／任务记录不受影响）。
- 修复方向：发布序号在 AppState/发布器层**单调推进**，不随切换任务或投影重建归零；卡片自身版本与全局发布顺序分开；保留单写者与原子提交；**重放不逐个发布历史卡片**。
- 停止：同一 AppState、真实临时目录连续完成 A/B/C，真实读回 latest 为 C；人为反转异步写入完成顺序，旧快照不覆盖新快照；重复重放不发布旧任务。

### R5 — 遗漏检查的结果必须进入失败统计（P2，与 R4 同片）——已关闭（2026-09-16，`d30f2956`）
- 落地回执：[R1_R5_FIRST_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R1_R5_FIRST_BATCH_RECEIPT.md)。失败在事件处、容量裁剪前计数（`failed_checks_total`）；显示行把账目与窗口分开（「N recorded, M FAILED (of which K in the 32-row window), J not shown」）；原访问器更名 `failed_checks_in_window`。
- 现状：`failed_checks()` 只统计仍在 `checks` 数组中的失败，`format_result_lines` 却把它与 `total_checks()` 并列显示 → 32 成功 + 第 33 个失败显示为 `33 recorded, 0 FAILED, 1 not shown`，遗漏可见但失败口径错误。
- 修复方向：总成功/失败数在**显示裁剪之前**按事件身份结算；显示列表只是摘要窗口；或明确写成"已展示的 N 项中失败 M，另 K 项未显示"。失败信息可优先保留，但不得修改真实执行结果，也不改 Core 验收规则。
- 停止：第 33 项失败计数正确；连续多项遗漏失败正确；重复事件与任务切换不重复计数。

### R6 — 普通输入也要进有序提交通道（P2，接续原 U6，agent-tui）——已关闭（2026-09-16，`a0f23d72`）
- 落地回执：[R6_R7_R8_B3_SECOND_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R6_R7_R8_B3_SECOND_BATCH_RECEIPT.md)。普通文本改走 `SessionCommand::Input` 同一有界通道；`CommandLane` 带未开始计数，`run_session` 持有 worker `JoinHandle`，退出时停收＋取消＋报数（转录那份可见性有限）。**限制**：路由改动无直接红用例，顺序断言在 lane 层、路由由 e2e 覆盖。
- 现状：`SessionCommand` 已覆盖任务切换/继续/挂起/保存/恢复，但**普通非 `/` 文本仍走独立 `tokio::spawn(handle.user_message(…))`**：worker 在等慢 checkpoint 时输入 `/task B` 再输入普通纠正文本，普通文本会绕过 worker 先到达当前任务 A。
- 修复方向：具有用户语义顺序的普通文本与任务切换**共享同一有界通道**；依赖任务身份的纠正优先用既有 `expected_task_id`/steering 入口表达目标；紧急取消可保留独立入口，但**必须定义它如何处理尚未提交的队列**；worker 的 `JoinHandle` 由 session 持有，退出时停止接单并明确取消/结算未开始命令（不能只丢弃发送端就当已取消）。
- 停止：真实键入顺序（`/task B`→普通文本、`/restore`→普通文本、取消与排队输入、退出时队列非空）每个输入都有明确的提交或拒绝回执；不把所有操作塞进一个会让取消排在慢 I/O 后的阻塞队列。

### R8 — 审批滚动与折行口径复用库语义（P2，接续原 U1/U2 渲染验收，agent-tui）——已关闭（2026-09-16，`a0f23d72`）
- 落地回执：[R6_R7_R8_B3_SECOND_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R6_R7_R8_B3_SECOND_BATCH_RECEIPT.md)。上限改用 `Paragraph::line_count`（经 `unstable-rendered-line-info`），手写 Unicode 表删除、改委托 `unicode-width`；新增真实 TestBackend 窄窗尾部可达与组合字符光标两例。**注意该特性上游标注为不稳定**，若变更只需改 `wrapped_line_count`。
- 现状：`wrapped_rows` 与 conversation 行数仍用 `display_width(line).div_ceil(width)` 估算，而 Ratatui `Paragraph` 按**词边界**折行（未填满即换行）→ 滚动上限被低估，长参数尾部或 sentinel 可能到不了；同处**手写 Unicode 宽字符表**把组合附加符/ZWJ 算成 1，不等价于 `UnicodeWidthStr`。
- 修复方向：复用项目锁定比例版本的宽度/布局语义——评估该版本受特性门控的 `Paragraph::line_count`，或**先统一折行一次、渲染不再二次 Wrap**；不要为减少修改文件数维护第二份 Unicode 与折行算法。
- 停止：真实 `ui::render` + `TestBackend` 覆盖窄宽度、大量不能同行的单词、组合字符/ZWJ、中文、缩进与尾部 sentinel；不只比较字符串或宽度辅助函数。本轮**未执行**这些后端渲染反例，不得把静态差异当作已测得的像素结果。

### B3 — ledger 导出取消安全——已关闭（2026-09-16，`3fe396c9`）
- 落地回执：[R6_R7_R8_B3_SECOND_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/R6_R7_R8_B3_SECOND_BATCH_RECEIPT.md)。导出改为「快照→提交→确认消费」，取消不再丢行；`ContextLifecycleRecord` 增 `PartialEq/Eq`。
**B1（`de6bf061`）与 B2（`2482f3ef`）亦已关闭（2026-09-17）**——回执：[B1_B2_THIRD_BATCH_RECEIPT](reviews/2026-09-16-review-3bdb269c/B1_B2_THIRD_BATCH_RECEIPT.md)。本阶段队列至此只剩 C（续）与 T8。

## 第七批（`6afa25df` 续审：QA/QB/QC/QD 四切片）——已全部关闭（2026-09-17，回执：[QA_QB_QC_QD_RECEIPT](reviews/2026-09-16-review-6afa25df/QA_QB_QC_QD_RECEIPT.md)）

审查基线 `6afa25df`（报告：[REVIEW.md](reviews/2026-09-16-review-6afa25df/REVIEW.md)，实施任务：[NEXT_ACTIONS.md](reviews/2026-09-16-review-6afa25df/NEXT_ACTIONS.md)，覆盖表：[COVERAGE.md](reviews/2026-09-16-review-6afa25df/COVERAGE.md)）。共同主线：**准备成功≠消费提交成功；连接可用≠事件流健康；业务拒绝≠没有费用。** 审查环境未执行回归（无 Cargo/dotnet），红-first 由实施补；`6afa25df` 的 CI run `35134020808` 审查读取时 attempt 1 进行中，不借用父提交结果。O1 与 O2 后续小切片已关闭（2026-09-17，回执：[O1_O2_RECEIPT](reviews/2026-09-16-review-980bbc77/O1_O2_RECEIPT.md)）。KV 本地序列验收表见审查 NEXT_ACTIONS（与切片并行准备，真实端点仍 NOT_RUN）。

### QA — 冷正文从预览到消费成功（Q1，context-simple＋Runtime 集成）——已关闭（`c62ce4fe`）
用户动作：固定小热目录下 A/B/C 必需正文进入请求并继续任务，无需扩大热预算。
- 现状：B1 捕获让 A 驻留冷页仍进最终帧，但 `acknowledge_consumption` 的 `has_exactly_one_owner` 只统计 heap/Warm/retry/已加载 external；`access::stamp(_consumed)` 无 pending cold 更新路径。Runtime 的 ACK 直接带最终 `materialized.items` 全部 ID → ACK 拒绝有效消费，本轮结果不提交。
- 方向：统一逻辑 owner 查询覆盖四类已加载 owner＋冷定位，并绑定条目/卡片版本（"pending 里出现过同 ID"不充分）；消费结算给本次已验证、实际送入请求的冷正文记录有界访问事实。不许永久 pin、扩热预算、全量重水化；不取消预览身份与 owner 校验。
- 验收：延长 `batch_required_plan` fixture 到真实 `ContextConsumptionAck`（先红：旧计数拒绝 A）→ 真实 Runtime 成功结果提交 → checkpoint/restore 一致；错误 materialization ID、外来 ID、过期卡片版本仍被拒绝。

### QB — 一次模型尝试只有一个使用量结算出口（Q2＋Q3，agent-runtime＋provider-openai）——已关闭（`559c6638`）
用户动作：模型已返回计数后，流失败/内部提交失败时正式账目仍保留这些数值。
- Q2：`OperationOutcome::ModelOutput` 分支先提交 ACK、失败即返回，`ModelUsed` 发布在其后——ACK 失败丢已知用量。用量结算须从业务接受分支提取，复用 W4/R2 既有通道与 operation 去重；ACK 失败仍拒绝工具派发与不可靠结果，不放宽校验。
- Q3：流循环 idle timeout、流/行/帧上限、I/O、解析/accumulator 错误、部分 sink 错误等裸提前返回绕过尾部 usage 提取。一次 attempt 的读流与结算拆开：所有退出统一从该 attempt 的 accumulator 取已知数值；错误分类/可重试性/取消语义不变；同 attempt 累计快照不重复相加，多 attempt 才合计。
- 验收：受控 Provider 已知 usage＋ContextEngine ACK 注入失败 → 工具不执行、错误如实、用量入账一次（不依赖 metrics env）；本地 SSE fixture 在 usage 后分别注入 idle timeout/坏帧/I/O/cap/sink error 断言 reported_usage，无 usage 的同形错误保持 Unknown；经真实 RetryingTransport 首次失败＋最终成功各计一次。

### QC — SDK 事件流健康与重同步代际屏障（Q4＋Q5，clients/dotnet）——已关闭（`2221ecec`）
用户动作：客户端自身落后导致 Session 队列溢出、底层 socket 仍健康时，能从公开入口恢复实时事件。
- Q4：`PumpEventsAsync` 溢出后关队列置 `_eventsOverflowed` 退出但不使连接失效；`LiveAsync` 见 `IsConnected` 即复用原连接——Snapshot 可用而事件流已死。把 Session 流健康纳入可用性/恢复判断或提供显式重建 snapshot+subscription+pump 的恢复动作；未知结果 mutation/审批不自动重发；旧 reader 保持终态。
- Q5：两个竞争窗口——新连接安装后先启 pump 后 `Resynced(snapshot)`（快照可覆盖新事件）；旧 pump 锁内校验、锁外入队（旧通知进新视图）。连接身份＋事件队列＋generation 作为同一发布边界：快照屏障先于新 pump 交付；旧 pump 最终入队受代际边界约束。用受控暂停点验证，不用 sleep。
- 验收：复用 `Overflowed_session_rebuilds_its_event_stream_on_reconnect_and_delivers_new_events`，去掉服务端 dropFirst、保持查询可用：公开恢复路径带来新快照、新 reader、下一条事件；正常断线重连不回归。

### QD — TUI worker 正常/异常退出共同收尾（Q6＋O3，agent-tui）——已关闭（`49209740`）
用户动作：绘制/键盘出错或用户退出时，前端已结束而排队动作继续提交的情况不再发生。
- 现状：`sink.draw(&app)?`、`source.poll_key(...).await?`、dispatch 早退绕过循环后的 `abort()+await`——JoinHandle 被丢弃即分离任务，worker 可能继续消费队列且会话失去 join 结果。
- 方向：会话循环 Result 与统一 cleanup 分开，异常/正常同一入口：停止接收→worker 停机回执→join→诚实结算（queued/已取走/已送 Runtime/未知分开，不把 abort 冒充未执行或回滚）；终端 guard、Runtime 清理、worker 各自责任不混；O3 的 pending 计数改纯诊断并修增减时机。
- 验收：worker 受控等待＋队列有待派发时，UiSink/UiSource 抛错与正常 quit 分别执行：stop 屏障后不偷派、worker 被回收、terminal 仍恢复、慢存储/已发命令的取消安全结算。

## 第八批（`980bbc77` 续审）——已全部关闭（2026-09-17，回执：[E1_E2_E3_E4_C_RECEIPT](reviews/2026-09-16-review-980bbc77/E1_E2_E3_E4_C_RECEIPT.md)）

审查基线 `980bbc77`（报告：[REVIEW.md](reviews/2026-09-16-review-980bbc77/REVIEW.md)，任务：[NEXT_ACTIONS.md](reviews/2026-09-16-review-980bbc77/NEXT_ACTIONS.md)，覆盖表：[COVERAGE.md](reviews/2026-09-16-review-980bbc77/COVERAGE.md)）。主线：**转换后的表示不能沿用转换前才成立的证明**——正文截断、整数规范化、编译与测试覆盖皆如此。上一轮 QA/QB/QC/QD 实现保留，不按旧问题重开。

### E1 — 输出裁剪与投影覆盖（P1，agent-workspace＋agent-runtime）——已关闭（`c6241318`）
用户动作：读大文件后，Agent 能辨别哪些正文真正在本次输入中；缺失中部不被历史去重隐藏。
- 现状：`FsReadTool::execute` 返回真实窗口事实；`WorkspaceOutputBroker::bound` 与 Runtime 兜底截断 `model_content` 却不更新 `covers_file`/`window_truncated`；`file_read_window_from_output` 据旧 metadata 判定完整窗口 → `omit_selected_file_body` 错误省略历史必要正文。
- 方向：**任何改变模型可见正文的可信转换必须同步重算/失效覆盖声明**；源版本身份不变；metadata 与总预算更新后再次满足约束；不解析截断提示文字、不以 `artifact_ref` 存在替代全文可见、不全禁用正确去重。顺手补 `truncate_with_marker` 在 budget≤marker 长度的小预算边界。
- 验收：真实跨层反例（历史中部短窗口→同版本全读超限→经纪截断→组装后 sentinel 不得报完整覆盖）；Runtime 兜底、正常未截断读、同版本不相交窗口对照。`cargo test -p agent-workspace --lib`、`-p agent-runtime --lib`、`-p agent-compose`。

### E2 — 参数语义与摘要域一致（P2，agent-contracts＋agent-core）——已关闭（`effd09aa`）
用户动作：两个实际可区分的长整数参数不共享同一 operation 参数身份。
- 现状：integer profile 用 `as_i64` 校验，JCS `write_number` 经 `as_f64` 序列化——n=9007199254740992/…993 同摘要；Core 用该摘要做准入/发布/执行身份而派发携带原始参数。
- 方向：明确数值域并文档化（拒绝非无损整数或收紧安全子集；长整数走版本化 schema 字符串）；授权、摘要、执行消费同一语义值；`minimum/maximum/enum` 同域；保留既有持久摘要兼容，不加 `expect` 把拒绝变 panic；常规 1/1.0 等价、键排序、历史向量按既有契约保持。Core 权限/意图检查不动。
- 验收：`SchemaProfile → ArgumentDigest → Core 参数绑定` 全链，覆盖 2^53 附近、正负非精确整数、跨语言向量。`cargo test -p agent-contracts`、`-p agent-core`、`-p agent-platform-protocol`。

### E3 — Linux 分片纳入 agent-host 测试（P2，CI）——已关闭（`7b48f97c`）
`cargo test` 分片并集漏 `-p agent-host`（check/build 不等于运行其测试）。把 agent-host 纳入负载合适的 Linux 分片，确认 sibling fixture 构建要求；用 workspace metadata 做「预期测试包集合 = 分片并集＋显式排除」的轻量一致性断言。Ubuntu .NET job 的宿主跨进程测试照旧存在，不夸大缺口。

### E4 — mkfifo 测试的 CString 修正（P2，测试设施）——已关闭（`7b48f97c`）
`agent-workspace/src/runtime_facts.rs` 的 Unix 测试把未 NUL 终止的字节传给 `libc::mkfifo`。改 `CString::new(OsStrExt::as_bytes())`＋保持分配存活；核对创建文件是预期位置的 FIFO 后再跑原不阻塞回归。本地以 `cargo check -p agent-workspace --target x86_64-unknown-linux-gnu --tests` 编译校验，实际运行归 CI。

### C（续）— 实际请求序列的供应商 KV 与任务总成本——本地阶段已关闭（`9d798c44`）；端点侧归 T8
复用既有 HTTP 捕获设施走连续真实请求：固定任务/工具契约/profile/预算，依次引入状态计数变动、合法新证据、同版本不同窗口、文件修改、工具撤销、协议 checkpoint、取消后迟到用量；对比最终 wire 的边界 digest、首差异类别与正文完整性；E1 大文件裁剪场景必须纳入（E1 合入后补跑）。本地阶段证明 layout/wire/账目语义；真实端点 accepted/hit/收费保持 NOT_RUN。

### T8 相关——KV 本地序列验收（待做，与 T8 同线）
固定任务/profile/工具契约/窗口预算，用最终请求逐轮只改一个原因（焦点、新检索、缺失提示、文件版本、工具撤销、checkpoint 恢复），核对边界摘要、首变原因与 usage 完整性。QB 未修时失败路径漏计已知成本，不据其宣称降本；真实端点接受/命中/净费用保持 NOT_RUN。

### T8 — 条件性供应商成本对照
**先有 R2 的完整结算**再测真实供应商，否则只比较最终成功调用会漏掉失败/取消成本（取消频繁的长任务成本可能被低估）。场景固定同一任务/起点/验收，至少覆盖前缀稳定、动态尾部、文件版本变化、checkpoint、维护、失败重试与取消补账；报告 uncached/read/write/output、主/维护调用、尝试数、未知覆盖与任务质量。无授权/凭据/预算则 `NOT_RUN`，不借用环境密钥发起付费实验。

## 第九批（`71f8a586` 续审）——已全部关闭（2026-09-17，CI run `35169239599` 全绿，回执见各条目）

审查基线 `71f8a586`（报告：[REVIEW.md](reviews/2026-09-16-review-71f8a586/REVIEW.md)，任务：[NEXT_ACTIONS.md](reviews/2026-09-16-review-71f8a586/NEXT_ACTIONS.md)，覆盖表：[COVERAGE.md](reviews/2026-09-16-review-71f8a586/COVERAGE.md)，CI 摘录：[CI_OBSERVATION.md](reviews/2026-09-16-review-71f8a586/CI_OBSERVATION.md)，schema 回归规格：[SCHEMA_CASES.json](reviews/2026-09-16-review-71f8a586/SCHEMA_CASES.json)）。主线：**返回的"继续"必须真的可执行、位置正确；已接受的约束不能被编译丢弃；取消要覆盖正在发生的写 I/O。** CI run `35156892711` Windows full Rust test 已实际失败（C0），不与本轮静态发现混为同一根因。审查环境未执行 Rust 测试，红-first 由实施补；上轮 E1–E4、O1/O2、C 线修复保留不重开。GUI 后置不变。

### C0 — Windows 验证进程树清理失败（agent-compose＋tool-runtime）——已关闭（2026-09-17，`2e70efc9`）
落地回执：[C0_PROOF_SUPERVISION_RECEIPT](reviews/2026-09-16-review-71f8a586/C0_PROOF_SUPERVISION_RECEIPT.md)。根因因果确认：`crates/tool-runtime/src/tools/process.rs` 里子进程 spawn 与 `AssignProcessToJobObject` 之间存在窗口，窗口内出生的 member 不入 host-death job——宿主被 TerminateProcess 后 KILL_ON_JOB_CLOSE 只杀已入 Job 的 leader，member 在 Job 外存活到自身超时。注入 25ms spawn→assign 延迟 3/3 复现 CI 同签名 panic（同 panic 行、leader `Ok(Exited)`、member 同 token `Ok(Running)`）；修复（`CREATE_SUSPENDED` 创建＋assign 完成后确认 resume；不可确认则杀树＋有界确认死亡＋类型化拒绝，fail-closed）后同注入 3/3＋2/2 绿、proof_supervision 10/10 绿。本机不自发复现（16 核低载），远端同二进制哈希 20:52 过/22:26 挂证明间歇性，与窗口竞态一致。残余（同缺陷类、不在本次失败路径，留后续同模式收口）：agent-process `create_job_object` 与 `integrity.rs` 的 spawn→assign 窗口。
原始现状：run `35156892711` job `104999146984`，`proof_supervision.rs:116` 报 leader 已 `Exited` 而同 `identity_token` 成员 20 秒仍 `Running`；测试用真实 `crash_child` 主动杀宿主，identity 检查防 PID 重用。

### F1/F2/F6 — 取回切片：可继续、位置正确、覆盖诚实（tool-runtime）——已关闭（2026-09-17，`33d797d5`/`8053c3b0`/`a6bd208c`）
落地回执：[F1_F2_F6_RETRIEVAL_RECEIPT](reviews/2026-09-16-review-71f8a586/F1_F2_F6_RETRIEVAL_RECEIPT.md)。F1：`end_line` 改 `Option<usize>`，缺省按 `start_line` 派生 `start..start+199`（checked，溢出干净拒绝），显式范围与 EOF clamp 语义不变，生成/解析同源。F2：新增 `line_byte_offset` 行内原始字节游标（绑定既有 artifact reference，UTF-8 码点边界切割；红例在真实 broker 16k 裁剪后的正文上 verbatim 回放续读参数——经纪截断后仍可继续）；`has_more`/`window_truncated`/EOF 声明一致，截断行尾部不再被跳过。F6：partial 扫描的停止原因/未搜索候选数/显示上限进 coverage footer 正文，不发假 continuation。红绿对照：F1 修复前 2 红、F2 前 7 红、F6 前 3 红，修复后 `cargo test -p tool-runtime` 290/0＋agent-conformance 全绿；变异恢复法承重。限制：8 MiB 扫描预算外仍只读已扫前缀（既有取舍）。
用户动作：按工具给出的参数一路读取工件（含超长单行尾部）；符号搜索不完整时执行者能从模型正文知道未覆盖范围。
- F1（`crates/tool-runtime/src/tools/artifact.rs`）：`default_end_line=200` 与 footer 只建议 `start_line=next` 矛盾——照返回参数调用即得 201–200 `invalid line range`。缺省 `end_line` 时按 `start_line` 派生有界终点（checked arithmetic），或返回完整合法继续参数；生成与解析共享类型化语义，兼容旧显式 `end_line`。
- F2（同文件）：单行超过剩余 capture（≈2 MiB/次、8 MiB 扫描预算）时只存前缀补换行却把 `last_captured_line` 推过整行，`has_more` 不含 `captured_truncated`——3 MiB 单行报 `end of artifact` 且尾部不可达。给超长单行加**绑定不可变工件身份**的字节/行内游标，只有实际展示/消费的位置才推进；UTF-8 边界与原始偏移对应；经最终输出经纪（≈16k）再验证，不出现"预览工件递归替代原尾部"。
- F6（`crates/tool-runtime/src/tools/code.rs`）：`scan_incomplete` 只进 metadata，模型正文把"扫完没找到/只扫了部分/结果被截断"压成近似相同体验。沿既有 coverage footer 报告已扫描范围、停止原因与结果上限；无真实扫描续跑不发 continuation，诚实建议缩小目录/条件。
- 回归红例（先红后绿）：450+ 行 fixture 用**真实工具返回的 continuation 原样连读**到 sentinel（测试不得代补产品未返回的参数）；3 MiB 单行后半 sentinel 可达、行内游标单调推进；空命中但扫描未完的正文提示；CRLF/UTF-8 多字节/字节上限边界。全部经 producer→broker→最终 tool message 检查。
- 停止：所有返回继续参数可执行、EOF 有真实依据、小型完整输出保持原语义；不引入向量检索/AST 服务/新存储引擎。

### F3/F4 — 参数契约切片（agent-contracts＋agent-core）——已关闭（2026-09-17，`aa83f65e`/`d84d6ee8`）
落地回执：[F3_F4_CONTRACT_RECEIPT](reviews/2026-09-16-review-71f8a586/F3_F4_CONTRACT_RECEIPT.md)。F3：`BoundedNode::Bool/Null` 携带 `enum_options` 走 `check_enum`（`enum:[false]` 拒绝 `true`，Core 层测试证明拒绝发生在审批 0 次与 dispatch（PanicDispatcher 不触发）之前）；空 `enum` 编译失败；无 type 节点的 shape 约束（pattern/properties/required 等）改 admission 类型化拒绝（fail-closed——外部 MCP 工具此类 schema 移出 surface 并记 diagnostics），typeless `enum` 与注解保留。F4：边界改 `(-5..=0)`（−6<n≤0），plain 分支重写为严格 ECMA-262 `Number::toString`；阈值两侧＋1e21 两侧＋Appendix B 32 向量（本机 Node 实测生成期望值）＋11,995 采样 bit-pattern 差分 0 mismatch。摘要兼容沿 E2 模式：摘要每次从内存参数派生、恢复重放持久字节不重算，历史 WAL 不动。`cargo test -p agent-contracts` 198/0（+8）、`-p agent-core` 172/0（+17）；变异恢复承重。
- 原始要点：`NodeType::Bool => BoundedNode::Bool` 丢弃已检查的 `enum_options`；无 type 嵌套 schema 约束退成 `Any` 静默弱化；`(-6..0)` 使 `1e-7` 输出 `0.0000001` 而 JCS/ECMAScript 应为 `1e-7`。
- F3（`crates/agent-contracts/src/schema_profile.rs`）：`NodeType::Bool => BoundedNode::Bool` 丢弃已读取并检查过的 `enum_options`——`{"type":"boolean","enum":[false]}` 接受 `true`；无 type 嵌套 schema 的 `pattern`/`properties`/`required` 退成 `Any` 静默弱化。保留独立约束或**在 admission 明确拒绝不支持组合**；编译 profile、渲染给模型的 schema、dispatcher gate 三者同义。回归走 compile→validate→Core no-dispatch：`flag=true` 对 `enum[false]` 拒绝且不进审批/执行，合法对照通过；用例规格见 SCHEMA_CASES.json。E2 已修的非精确整数域不重开。
- F4（`crates/agent-contracts/src/jcs.rs`）：`scientific_from_ryu` 的 `(-6..0).contains(&point)` 使 Ryu 的 `1e-7`/`1.2e-7`/`-1e-7` 被写成 `0.0000001` 等，而 JCS（ECMAScript 数值格式）应为 `1e-7` 等（Node 实测对照在 MECHANISM_CHECKS.json）。修正 plain/scientific 开闭边界；补 Appendix B、指数阈值两侧、正负值、有限浮点 bit-pattern 差分。`ArgumentDigest` 已用于 Core 参数身份——历史持久摘要需明确解释/兼容策略，**不放宽摘要校验、不批量重算旧 WAL**。
- 回归：`cargo test -p agent-contracts schema_profile`、`-p agent-contracts jcs`、`-p agent-core`。schema mismatch 不得到达审批与执行。

### F5 — 通用进程发送阶段取消（agent-process＋agent-capability-process）——已关闭（2026-09-17，`d336e5ef`/`e27d13bb`）
落地回执：[F5_PROCESS_SEND_CANCEL_RECEIPT](reviews/2026-09-16-review-71f8a586/F5_PROCESS_SEND_CANCEL_RECEIPT.md)。`FramedProtocolSession::send_bounded` 统一首次请求/broker 答复/cancel 帧三处写入的期限＋取消（biased select、取消优先），`Abandoned` 如实表示半帧并 poison＋kill_tree，不对对端是否执行作断言；等待发送权（transport Mutex）纳入同一期限；写前取消零字节过线、连接可复用的既有语义有守卫测试。真实 `mock_host` 新增停读模式（消费恰好 N 字节后停读，快照即部分写入证据）：256 KiB 帧阻塞写＋取消实测 ~0.6s（对照 30s 超时）。`--test host` 28/28、capability_process 26/26、conformance adapter_fault_matrix 11/11；三处变异复验。限制：锁等待期限分支无直接测试（防御性收口）；一次共享 CPU 下的偶发超界经负载对比确认非语义回归（22 连绿）。
原始现状：`exchange_once` 首次请求与 broker 答复直接 await `send_encoded_line`，取消检查未覆盖写/flush；子进程停读时只能等外层 30s 超时。

### KV — 生产装配序列验收（agent-compose 测试层）——已关闭（2026-09-17，`21dece2b`）
落地回执：[KV_SEQUENCE_RECEIPT](reviews/2026-09-16-review-71f8a586/KV_SEQUENCE_RECEIPT.md)。新增 `crates/agent-compose/tests/kv_production_sequence.rs`：同 TaskId/workspace、15 轮 HTTP/7 回合，真实 Compose/Actor/OpenAI Provider/内置工具驱动（脚本式 SSE 只决定决策）——真实读取＋fs.write（磁盘逐字节断言 evidence.txt，修正既有 smoke read_only 证明不了写入的缺口）→新证据→`steer_active_task` 焦点改变→文件版本覆写→`capability.manage` 工具撤销→checkpoint/真实重组/restore→`response.failed` 失败结算。key 恒等且=`routing.key_for(task,"main")`、B0 逐字节钉位、首差异可归类、账本 15 行精确各一次；4 次运行全过（9.6–9.9s）。既有 smoke/手工矩阵未退化。**LOCAL_WIRE=PASS；ENDPOINT_ACCEPTED/SERVER_HIT/NET_TASK_COST=NOT_RUN** 归 T8。观察（记录非缺陷）：`context_manage` 随 NeedEvidence 租赁在相邻轮间进出、每轮改变 tools 并使供应商复用边界失效——T8 供应商实验需单独核对。

### C1 — checkpoint 分片墙钟预算满载抖动（CI 阻塞续查，context-simple）——已关闭（2026-09-17，`8996ffeb`）
落地回执：[C1_CHECKPOINT_SPILL_RECEIPT](reviews/2026-09-16-review-71f8a586/C1_CHECKPOINT_SPILL_RECEIPT.md)。CI run `35158964457`（基线 `7631dd72`，C0 被挡住后露出）Windows 分片三测失败：`external_spilled` 计数短缺（15/20、19/20、12/14），Linux 同 run 全绿、本机 0.26s 全绿。根因：checkpoint capture 卡片写入循环的 `external_checkpoint_io_budget_ms`（默认 2s，`engine.rs:1462` 任一迭代越线即停）在满载 runner（该二进制 401s，慢约 40 倍）中途耗尽，剩余计划卡片本次静默留 inline——**屏障完整性从不依赖 spill**：截短后剩余条目以全量元数据内联进 checkpoint 值（F2 成文契约），恢复完整无损、跨 capture 收敛；两个 shortfall 不同排除写入被吞与测试互踩。修复沿 cold_bounds 先例（run `3499986097` spilled 38/40 → 钉宽）：给漏钉的两个 fixture（`spill_config`、`carded_history`）钉 `external_checkpoint_io_budget_ms: 60_000`，生产代码零改动；并补此前缺失的确定性回归 `an_exhausted_capture_io_budget_stays_inline_and_converges_on_later_captures`（预算注入 0 三测同签名转红；生产臂注入 `&& false` 变异转红→恢复）。`cargo test -p context-simple` ×3 442/0；生产默认 2s 预算保留（真实满载盘语义不变）；CI 终验 run `35169239599` Windows 分片 ✓（19m27s 满载）。
## 第十批（`c8a62355` 续审）——已全部关闭（2026-09-18，本地集成回归全绿；CI 终验 run `35278745998` attempt 2 全绿，见 CURRENT.md）

审查基线 `c8a62355`（报告：[REVIEW.md](reviews/2026-09-18-review-c8a62355/REVIEW.md)，任务规格：[NEXT_ACTIONS.md](reviews/2026-09-18-review-c8a62355/NEXT_ACTIONS.md)，覆盖表：[COVERAGE.md](reviews/2026-09-18-review-c8a62355/COVERAGE.md)，机制探针：[MECHANISM_RESULTS.json](reviews/2026-09-18-review-c8a62355/MECHANISM_RESULTS.json)）。主线：**逻辑 owner 不随驻留位置变化；continuation 不越过尚未交付给模型的源位置；journal 写者身份不随 WAL 代际变化；必需隔离在目标代码运行前建立。** 四条不变量各只有一个生产维护入口；上轮 F1/F2 动态 `end_line`、行内偏移与 C0 挂起创建修复保留不重做。审查环境无 Rust 工具链，红例由实施补；实施环境为 Windows＋cargo 1.97.1，Unix 专属分支按平台语义单独记账、不冒充已执行。

### G1 — 逻辑 owner 驱动 reconcile（B，context-simple）——已关闭（`62b20e4b`）
回执：[G1_OWNER_RECONCILE_RECEIPT](reviews/2026-09-18-review-c8a62355/G1_OWNER_RECONCILE_RECEIPT.md)。owner 快照补齐 `pending_external_cards`＋commit 全 owner 位置复核；未读冷卡片＝详情未知≠无主；真孤儿接纳走既有 `settle_metadata_residency` 单一结算。审查反例红→绿（热 1→3＋pending 2 重复 → 每 id 恰一 owner、卡片版本保持），真孤儿正对照、重复 reconcile 幂等、checkpoint/restore 往返全过。context-simple **448/0**（基线 442＋6），clippy/fmt 干净。残余（非本片恶化）：新接纳孤儿无卡片时自身不能降级、报告类型化记账；双并发操作竞态由 commit 复核覆盖无专门竞态测试。

### G2/G3 — 最终模型预算内的连续交付（A/B，tool-runtime）——已关闭（`d1d5c8dd`）
回执：[G2_G3_DELIVERED_POSITION_RECEIPT](reviews/2026-09-18-review-c8a62355/G2_G3_DELIVERED_POSITION_RECEIPT.md)。新增共享 `tools/page.rs` 单一维护入口：`FINAL_BODY_CHARS＝MAX_TOOL_MODEL_CONTENT_CHARS`（16k）一处定义，工具按最终正文预算分页并同源预留 header/footer/行号/分隔符；`finalize_within_budget` 从保留 span 推导游标与交付声明——游标永不越过未交付源位置；`DeliveredSpan` 真实源行号（重编号结构性不可能）。扫描/捕获/交付三位置分别表达进 metadata。G3：capture 在第一个不可展示位置关闭，不越空洞接纳后行；fs.read 同缺陷形态（KV 轨迹证实）一并修复：`lines=S-E/N` 声明只描述实际交付区间、新增 `has_more`/`next_start_line`/正文续读子句、超页预算的行显式声明跳过。artifact 三探针（500 行日志、3 MiB 单行、UTF-8 反例）＋fs 四探针红→绿；既有 F1/F2/F6 与 fs 19 测不改保持。tool-runtime **299/0**、agent-workspace 125/0、conformance 35/0、`core3_restore_snapshot_paging` 绿。经纪（兜底）未改。残余：fs.read 超预算场景 metadata start/end 语义＝交付区间（装得下时不变），下游 `FileBodyWindow` 精确度保持或提高。

### G4 — journal 生命周期锁（C，agent-storage）——已关闭（`b0652cde`）
回执：[G4_JOURNAL_LIFECYCLE_LOCK_RECEIPT](reviews/2026-09-18-review-c8a62355/G4_JOURNAL_LIFECYCLE_LOCK_RECEIPT.md)。写者独占绑定稳定 `<base>.lock`（不随代际轮换、**读 metadata 之前**取得、持整个生命周期、不被 unlink 重建）；候选代际 `create_new`／先锁后清，旧的先截断写入后取锁流程移除；每代 WAL 锁保留为纵深防御；Workspace 外层锁未动未削弱。barrier 交错反例红→绿（旧代码：陈旧 opener 对已删除仍打开的 G1 句柄取锁成功、按旧 metadata 返回健康写者）；竞争压缩拒绝时候选逐字节不变（旧代码红：24→0 截断）；同进程双开类型化拒绝正对照（Windows 执行）。agent-storage **30/0**、agent-workspace 125/0；cfg(unix) 镜像仅编译验证未执行，Windows 同进程 LockFileEx 变体已执行。

### G5 — Windows containment 入口收敛（A，agent-process）——已关闭（`c8b0f3b3`）
回执：[G5_CONTAINED_SPAWN_RECEIPT](reviews/2026-09-18-review-c8a62355/G5_CONTAINED_SPAWN_RECEIPT.md)。新共享入口 `contained_spawn.rs`（CREATE_SUSPENDED→必需 Job 关联→确认恢复，fail-closed：关联失败 kill＋有界确认死亡＋类型化拒绝）；generic `ProcessHost::connect` 与 Low-IL `run_wrap` 都经它，忽略关联结果的 `let _ = assign_pid_to_job` 移除；attestation 来自"首指令前关联确认＋恢复确认"，不来自"创建了 Job 对象"。注入 300ms 延迟红→绿（后代逃逸、双存活变体即 C0 快机签名）；每生产入口的后代随宿主死亡、强制关联失败无 runnable child、正常路径控制全过（env-seam 强制失败经真实生产入口）。agent-process **86/0**、capability-process **58/0**；compose `proof_supervision` 绿。行为变化如实：必需关联被拒从降级改 fail-closed（tool-runtime C0 路径仍降级——分歧已记录）；`resume_suspended_process` 在两 crate 重复待集成统一。

### C（续）— KV 生产序列验收完整性（C，agent-compose 测试层）——已关闭（`1680e181`）
回执：[KV_SEQUENCE_COMPLETENESS_RECEIPT](reviews/2026-09-18-review-c8a62355/KV_SEQUENCE_COMPLETENESS_RECEIPT.md)。九批断言全保留；三类补全：**(1) 完整稳定前缀**——每轮整个 `input[0..=B0]` 逐项对照基线＋相邻轮整段公共断点前缀逐字节一致＋轮首分歧仅限 B1 证据项，tools/schema 整块 canonical 比较；**(2) 真实交付证据**——轨迹 26 轮/8 回合，走读段由捕获服务器从工具返回的 continuation 子句逐字生成（实际 9 页 1→600），交付声明链无缺口无重叠、每页无截断标记、块 ID 按真实源行号在声明页交付（EXPECTED-RED 随 G2/G3 落地转绿）；**(3) 完整成本口径**——LedgerRow 扩展 cache 三桶（Known/Unknown，不补零）/attempts/retries/主辅 lane/typed 上报，期望行从实际服务派生、身份唯一不重复计、总额三方一致。KV 四连绿 **14.0s**；LOCAL_WIRE=PASS、ENDPOINT_ACCEPTED/SERVER_HIT/NET_TASK_COST 仍 NOT_RUN 归 T8。

### 第十批集成回归（2026-09-18，本地 Windows，全部实际执行）

`cargo test -p context-simple` 448/0；`-p tool-runtime` 299/0（1 ignored）；`-p agent-workspace` 125/0；`-p agent-storage` 30/0；`-p agent-process` 86/0；`-p agent-capability-process` 58/0；`-p agent-compose` 全套 0 失败（含 KV 14.0s、proof_supervision）；`-p agent-host` 31/0；`-p agent-core` 172/0；`-p agent-contracts` 198/0；`-p agent-conformance` 35/0；`-p agent-runtime` 744/0。`cargo fmt --all -- --check` 干净；`cargo clippy --workspace --all-targets -- -D warnings` 干净；doc gate OK。未执行：真实供应商实验（T8）、Unix 平台语义（cfg 门控仅编译）、GUI。

## T1–T7 完成记录（历史）

### T1 — 统一最终装箱与发布（B/C 共享，单一集成人）

结果：预算不足时优先保留真正必要的证据；模型实际输入与观测、覆盖和缓存边界一致。

修 R1（分区内选择可选项：selected 只剩必需正文时，会先于 foreground 中的可选大正文被裁，制造本可避免的 BudgetExcluded 必需缺口）＋ R2（撤销 settlement 投影重组请求后 input_total/packing_total 未重算，计数来自旧请求）。入口：`actor/model.rs`、`prompt.rs`、model/model_cache 契约。先修两缺陷，再沿 ModelRoundPlan/ModelInput 抽取稳定纯装箱函数：统一候选优先级→一次生成最终消息/工具/覆盖/计数/缓存计划。Actor 继续负责调度、身份检查与发布。

验收：R1 两分区反例（selected=必需小正文＋foreground=可选大正文、总输入略超预算→必需保留、无对应 required miss）；全必需确实超限时仍诚实报告；投影修订后计数与完整重算相等；无 over-budget 伪发送。停止：不改 Context 打分/GC 算法、不重做全部 prompt 文案、不新增事件数据库。

### T2 — 外置目录的增量维护（B）

结果：无变化的维护不反复付序列化和目录重建开销，冷恢复保证保持。

修 R3（零删除或仅延期的 storage GC 仍对全部 external 条目 take_all→replace_all，清空 card_hashes 并触发目录/索引重建；后续 checkpoint 可能重新序列化未变化元数据）。入口：`store.rs::commit_storage_gc`、`index/external.rs`。零删除零结构变更、仅更新必要观测；部分删除只处理真实成功/NotFound 的 id、保留其他条目有效 card claim；把针对单项的结构变更收敛到具名目录 API（状态迁移＋索引更新＋card claim 作废同入口），减少 get_mut 调用者隐式约定。

验收：已记录卡片的 N 条目→零候选 GC/不完整根延期→全部原 card claims 保持；部分删除仅对应 id 消失、后续 capture 不重新序列化幸存者；现有恢复根与 pending owner 反例全部保持。停止：暂不引入新数据库；真正冷驻留归 T4，不把 checkpoint 分片冒充内存分页。

### T3 — 有界进程与能力发现（A）

结果：长工具可持续轮询/停止，退出/输出/清理事实清楚；MCP 发现不超出声明预算悄悄成功。

修 R4（工具总数与总期限只在下一次循环入口检查：末页追加后可直接返回成功、末页请求可跨过总期限）＋ R5（MCP 的 reap helper 忽略 `ProcessReapOutcome::Unconfirmed` 并清空 supervisor，上层失去"清理未确认"的类型化事实）。入口：`mcp.rs::list_tools_with_cancel`、`supervisor.rs`。总量/累计字节检查放到接纳与最终返回前；剩余总期限传入实际交换边界；关键结算结果 `#[must_use]`，调用方明确传播/保留待结算/报告不确定性；游标循环用有界 seen set。Core 准入另有 MAX_TOOLS_PER_CAPABILITY=32——发现上限与准入上限是有意分层时须明确用户能否筛选。

验收：单页 513 工具无 nextCursor 仍拒；末页跨总期限仍拒；**用不同工具名或空页构造 cursor 循环**（不得让"重复工具名"拒绝抢先触发、实际未验证游标规则）；Unconfirmed 经可注入 supervision seam 传播。停止：不新增进程调度器、不扩展 MCP 协议范围替代已声明范围修复。

## 第二批（并行）——已全部完成合入 main（2026-09-15，T4 `1a851169`、T6 `dbde17e8`、T5 `9edd63f5`；各自本地全量绿＋clippy 0，远端 CI 待记录）

### T4 — 真的有界冷目录、搜索与维护工作（B）——第一期已完成（2026-09-15，`1a851169`）
每操作预算（items＋绝对 deadline）、类型化 `HydrationOutcome{complete,remaining,stop:Budget|HotCap|Unreadable}`、热上限（8192 条/32 MiB 软）落地；规模回归红→绿（5 倍超预算冷集合全程有界可继续）。第二期（持久冷目录分页、热/冷双向驻留、跨 crate typed 传播）沿原队列继续。原始任务：

结果：历史增长后，常用任务不因首次搜索或 GC 全量重水化历史而大幅停顿；未读区域始终可检索且不被误删。

沿现有 store/index/ContextEngine 实现持久目录或分页查询视图、有上限热元数据缓存、受保护 roots/pinned/dirty 条目的明确规则。先记录当前元数据规模与查询工作量再选实现，不先决定向量库或新服务。每操作分别定义 items/bytes/IO/绝对时间预算；超出返回可继续状态或类型化不完整，不把总工作量推给隐藏的 hydrate_all 循环。搜索区分"排名 Top-K"与"因未读页/故障覆盖不完整"（类型化差异）。`run_storage_io` 的派发后汇集模式使累积完成结果与候选计划总量无界——与单批总工作量设计一起收。

验收：明显大于热预算的冷集合；恢复/按 id fetch/搜索/GC 往返后热资源不随全历史永久增长；未读依赖闭包时延期删除；候选排序/结果稳定性有版本依据。停止：第一版只证明声明规模下的边界，不宣称无限历史、任意故障常数时延。

### T6 — 供应商 KV 的稳定布局（C，依赖 T1）——第一期已完成（2026-09-15，`dbde17e8`）
R6 已修（`rc2-`＋64 hex 摘要，一次性路由迁移已记录）；官方 content-block 形状与当前 item 同级形式的差异固化为 `endpoint_shape_tests` fixture（翻转断言归 T8 端点核验）。第二期（三层稳定布局落地、CachePlan 收敛）沿原队列继续。原始任务：

结果：连续请求复用有效、预计重复使用的前缀，减少无谓缓存写入，不延迟新事实或新指令生效。

先修 R6（路由键 `|` 拼接无转义可碰撞——`(a|b,c,…)` 与 `(a,b|c,…)` 同串；对带版本结构化 tuple 规范序列化后经 SHA-256/ContentDigest 生成定长摘要，不截断结构、不改每轮随机键；键编码变更即一次路由迁移，记录即可）。核对具体端点断点编码：本地捕获服务器不校验 schema，官方 Responses 走其文档定义的 content-block 形式，网关扩展单独声明；独立 schema/fixture 断言不复制 mapper 当前输出当"正确答案"。沿 EvidenceSplit/ModelInput 构建三层布局：稳定基座（稳定策略/工具契约/较稳定有效材料）→当前阶段证据（版本化正文与范围）→动态尾部（新检索/缺失/恢复/最新指令）；频繁编辑正文不强留稳定区。"epoch"须代表实际跨轮有效快照/布局规则，非每轮新块命名。派生布局字节计入内存预算，不持第二份无界历史；权限撤销/来源版本变化/用户移除要求/必需缺口/硬窗口即时失效。缓存计划收敛为经最终输入验证的单一 CachePlan（旧 prefix+tools 摘要 hint 与新裸断点列表不并行两套失效规则）。

验收：相同有效基座＋不同焦点/缺失/恢复信息保留相同基座；文件修改/工具撤销/任务切换确实失效；A→B→A 重组不冒称原有后缀自动命中；最终 wire 断点位置合法。可用无付费模型的请求序列完成；缓存收益归 T8。停止：不为命中率保留过时证据、不追求一次覆盖所有供应商。

## 组合点与收尾

### T5 — 一份有效产品配置到所有入口（A/C 共享）——已完成（2026-09-15，`9edd63f5`）
RuntimeServices 字段分组＋`from_parts` 唯一构造路径；`ComposeConfig::product_baseline` 一处定义产品公共默认，host 入口收敛为基线＋12 项差异；`build_context_engine` 文档诚实区分维护预算适用引擎（rolling 逐 pass 生效；dynamic 仅挂载决策）。特征化测试钉住冻结基线，无行为变化。原始任务：

结果：host、TUI/headless、SDK 与明确支持的 service 路径对当前策略、预算、权限、恢复和完成方式无隐含分歧。

复用 compose/services：产品与实验设置分组、共用默认构造和校验（RuntimeServices 的 new/try_new 重复初始化与实验 bool 汇合），入口只解析差异。正式 host 默认 Rolling 先保持；Dynamic 明确可启用 profile、回归范围与实际限制；正数维护预算/backoff 的适用引擎在 effective config 中诚实区分"不支持/未接入"与"默认"。验收：读取 effective config 与执行时实际选择一致；不支持的组合拒绝或显式报告；未知修改不自动重放；普通 final 与持久完成区别保持。停止：不换 GUI 技术、不加多工作区总调度、不任意改变公开 wire。

### T7 — 同一任务的完整后端开发流程（A 主持，三线共同）——已完成（2026-09-15，host_t7_journey）
一条连续轨迹（named pipe；unix 入口同款归 CI）：跨文件目标提交（幂等重试同 task）→ 脚本模型经 Runtime 真实 fs.write（wire 审批放行）→ 运行中 steer（Queued 槽，无第二任务）→ continue＋mid-flight cancel（类型化 TurnCancelled barrier）→ 正式 checkpoint → 宿主关闭 → **新 server 冷恢复同任务**（同 TaskId 重聚焦、run lineage 链接）→ 文件逐字节存活/无效果重放 → 继续完成 summary.md → 交付＋OperatorClosureOnly 操作员关闭（TaskCompleted 携带 final_output_digest）。故障变体：恢复后真实 fs 失败保持 Active/可重试、条件清除后同写入成功。host 全量 25 绿（t7 2），clippy 0。原始任务：

结果：一个真实跨模块代码任务在同一 TaskId、同一工作区、同一恢复 lineage 内完成——不是多组无关测试的成功行相加。

复用现有 host/headless 与测试 harness，受控模型决策＋真实工具/进程/checkpoint 文件驱动：提交→计划/验收约束→检索读取修改→超窗证据找回→验证→用户纠正→中断保存→退出进程→新进程冷恢复同任务→核对外部变化→继续完成→交付/等待操作员关闭。同任务遇慢维护或临时冷页故障不丢指令/owner；已发生效果不重放。service 模式按实际引擎能力作为明确变体。验收：最终产物、任务身份与输入 lineage、未决效果、清理状态、有效配置、证据与费用完整性均可核对；允许复用既有断言，不得拼接不同任务的成功行。停止：一条代表性综合流程及关键故障变体完成后即回功能开发，不变成无限新增门禁。

### T8 — 真实模型质量与任务全成本对照（C，条件任务）——已关闭（2026-09-18，本地授权付费实验；结论分立记账）

授权凭据到位（eval.env：`deepseek-flash @ api.deepseek.com protocol=chat`）。预检修一处探针缺陷（`888bc0ae`：doctor 数据面探针硬编码 16 输出 token 对推理型 serving 必然 `finish_reason=length` 误报失败；改读配置 cap 后转绿，端点真实接受＋完成小请求）。新增 `agent-compose/tests/t8_kv_live.rs`（ignored，真实花费实验，证据先行后断言）：固定任务（读种子 brief→写 summary，字节级 marker oracle）×三相位（冷任务／同工作区暖续／跨工作区重跑），每轮从类型化 `ModelUsed` 事件记全账本（input/output/cache hit/miss/attempts/retries/身份/lane）。两连绿（34.7s／42.6s，共 ~44 轮真实请求）。
分立结论：**ENDPOINT_ACCEPTED＝PASS**（全部轮次经 pinned 协议完成、零运行时失败、oracle 全中，所有行 `UsageIdentity::Observed`）；**SERVER_HIT＝OBSERVED**（DeepSeek 服务端自动前缀缓存真实命中：两轮实验每相位 hit>0，跨工作区重跑的第 1 个请求即 hit=1536——跨请求复用成立；同一任务内后续轮 hit 稳定在 1.3k–2.9k）；**NET_TASK_COST＝已记token口径**（逐相位 input/hit/miss/output/attempts 全量落账；**金额换算 NOT_RUN**——需该 serving 的价格表，不以猜测单价折算）。真实发现（KV 回执要求的 `context_manage` 租赁核对）：跨区相位模型连续切换 `capability.manage` 后紧随的一轮 **hit=0 全 miss**——能力租赁进出改变 tools 数组即打断供应商前缀复用，实测成立。
证据：[2026-09-18-t8-kv-live.md](walkthroughs/2026-09-18-t8-kv-live.md)（只含 MODEL/BASE_URL/protocol，无密钥）。边界如实：单任务单 serving 的走查记录与成本观察，非基准测试；缓存命中为供应商侧行为；金额对照、多 serving 横向、质量-成本曲线保持 NOT_RUN（需额外预算/价格表时另行开窗）。

## 已关闭（链接）

前阶段全部切片（文档入口迁移、B1–B3、B2、A1–A3、C1/C2、阶段旅程）的回执链接见 [CURRENT.md](CURRENT.md)「上一阶段成果」；正文不在此复制。

## 维护性验收尺度（随片适用）

- 修改同一条规则需要同步改几个位置？是否仍依赖调用者记住隐式约定（如 get_mut 不得动某字段）？
- 关键结算结果是否可被静默丢弃（`#[must_use]`/类型化传播）？
- 测试能否明确证明目标行为——反例不被其他拒绝分支抢先"证明"（如重复工具名抢在重复 cursor 之前失败）？
- 故障注入放 test-only seam 或隔离树；不在共享工作树改生产代码做红检查后靠记忆恢复（grace RED_CHECK 事故的教训）。
- 行为修改与大范围文件移动分开提交；不以拆文件数/测试数/覆盖率百分比为产物。
