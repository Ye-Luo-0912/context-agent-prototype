# 第十二批收口回执（BR1–BR7＋E-1）

审查基线 `bcacf41b`；文档基线提交 `b78a3c94`。全部修复在 Windows 本机实际执行（cargo 1.97.1／Python 3.12.3），红→绿均为实测。审查报告：[REVIEW.md](REVIEW.md)；任务规格：[NEXT_ACTIONS.md](NEXT_ACTIONS.md)。实施方式：B-1／A-1／C 线三 agent 并行（文件所有权互不重叠），A-2 按"同 crate 串行"规则在 A-1 之后实施。

## B-1 — 冷页语义更新义务（BR1，context-simple）——已关闭（`6e993a03`）

**结构**：`ColdIntentTargetView { bound_event_seq, candidates(cap 64＋truncated 标志), settled(cap 16＋overflow 计数) }` 挂在 Supersede/Verify 两个意图变体上，`#[serde(default)]` 兼容 H2 旧 checkpoint（旧数据 bound=u64::MAX，保持 H2 语义不悄悄收窄）。

- **因果上界（B-1a）**：记录时绑定 `state.event_seq`；`created_tick > bound` 的条目永不为该意图的目标——后来新建的要求不被更早的撤销作废。
- **逐目标结算（B-1b）**：`ColdIntentApplication::{NotTarget, Settled, AlreadyTerminal, Unresolved}` 四态；处理 A 不清 B；consumed 仅当 `!truncated && 候选全部解决 && settled 非空`；截断快照永不宣称完成（由环 cap 64＋`cold_semantic_intent_snapshots_truncated` 治理）。
- **记录时全文否定判定（B-1c）**：`has_retention_protection` 在记录点对完整输入执行一次，受保护消息根本不记录意图；应用路径拆出 `names_the_same_requirement_proven`（不含否定重判），4000 字符副本只承载正向证明——副本只能丢正向证据（保守共存），不能把「保留」翻转成「撤销」。
- **Verify 三态（B-1d）**：证据证明 → Settled 落终态；证据暂不可读 → Unresolved（保留、不消费、不落终态）；形状不匹配/超上界 → NotTarget；终态卡重入 → AlreadyTerminal 幂等入账。

**红→绿（6 条新回归，全部实测红点后修复转绿）**：

| 测试（`tests::cold_semantic_intents`） | 修复前红点 |
|---|---|
| `one_removal_intent_settles_two_cold_decisions_across_two_installs` | 第一批即整条消费（left: 0 / right: 1） |
| `an_intent_never_finalizes_a_decision_created_after_it` | 后来决策被旧意图作废（Superseded vs Live） |
| `an_unreadable_by_evidence_keeps_the_verify_intent_unresolved_until_readable` | 形状匹配即消费 |
| `a_tail_retention_revision_beyond_the_truncated_copy_keeps_the_cold_decision_live` | 截断副本上记意图并撤销 |
| `a_midway_checkpoint_restore_keeps_settled_targets_and_open_obligations` | checkpoint 无 target view |
| `intent_target_accounting_stays_bounded_and_honest` | 无有界快照/无诚实计数 |

**验收**：`cargo test -p context-simple` **464/0**（基线 458＋6）；clippy `-D warnings` 干净；fmt 干净。H2 既有 5 用例（五位置等价、跨任务保留、IoFailed 重试、restore 不复活、Verify 冷卡结算）全绿。

**限制（如实）**：目标宇宙是记录时点的有界快照，非全历史义务；候选快照超 64 截断（诚实计数），截断意图永不宣称完成；4000 字符副本仍作正向证明文本（被截掉的需求词只会漏结算，不会错误终态）；Verify 的 Unresolved 若证据永不恢复，靠 checkpoint→restore 重装路径或环容量收敛，未新增维护期扫描；已加载位置（heap/warm/loaded-external）的因果上界不在本片（BR1 限定冷页意图路径）。

## A-1 — 失败检查点的真实捕获结果（BR2，agent-runtime）——已关闭（`d1b75da5`）

**结构**：`SafepointCapture::{Captured{sequence, debt_basis}, Deferred{PrepareInFlight|WriteInFlight|NoActiveTask}, AlreadySatisfied}`；`captured_failure_obligation()` 只有 `Captured` 且 `debt_basis` 含 `FailedTurnYield` 才置 `captured=true`（lifecycle.rs 两个调用点：占道路径与 relay 路径，后者传真实 `captured` 而非硬编码 true）。

**重驱动**：全部挂在**既有结算点**，未新增调度器——`maintenance.rs` 的 `GcContinuation::SafepointFailure` 结算分支（`land_safepoint_write` ACK 退休旧 debt 之后重入 `resume_failed_turn_checkpoint`）与边界完成后的 pending tail 重试；`captured=false` 的尾部的重新进入时重新累债并再次尝试捕获。同一快照序列只退休自己冻结的 debt（不变量未动）；重驱动仍 Deferred 且无 relay 可挂时落到既有 `continuation_durability_gate` 围栏（未吞掉、未清 debt）。

**红→绿**：
- `occupied_gc_with_inflight_prepare_still_captures_failed_turn_debt`（组合 F10+F12，不取消）：**红**——修复前单个 CheckpointDurable(seq 2) → Warning（debt 未捕获）→ RecoveryRequired → TurnFailed，无新快照冻结 failed_turn_yield；**绿**——S1 落盘后重驱动生成冻结 failed_turn_yield 的新快照，两个 durable 序列递增、无 RecoveryRequired、TurnFailed 后 `continue_active_task` 正常 TurnCompleted 且续接请求携带新指令。
- `combined_boundary_failure_restore_keeps_identity_without_replay`（再次恢复同 TaskId）：**红**（同缺失捕获断言）；**绿**——冷恢复后首请求含新指令、指令修订号不变、计数探针证明 fs.read 全程只执行 1 次（工具结果未重放）。
- `occupied_gc_with_inflight_prepare_cancel_publishes_single_terminal`（取消变体）：恰一次 TurnCancelled、无迟到 TurnFailed；**守卫性质**（取消路径不经门禁，修复前后均绿，无红证据——如实记录）。
- 既有单条件测试（有旧 prepare 但 lane 空；lane 占用无额外 prepare）全绿。

**验收**：`cargo test -p agent-runtime --test turn` **158/0/1 ignored**；`--lib` 无回归。

**限制**：`Deferred(WriteInFlight)` 在第二调用点理论上可达但实测不可达（先经 `await_pending_checkpoint` 排空），按防御性编码保留；turn.rs/tools.rs 其余 7 个 `safe_point_resume_commit` 调用点忽略返回值，行为与改前逐谓词等价（仅判定顺序重排）。

## A-2 — 收敛诊断与文本收尾（BR3＋BR4，agent-runtime）——已关闭（`0c643a2b`）

**BR3**：`ObservationEvidence` 新增 `UntrackedNovelty`；分类顺序改为「普通合并 → 满载时无损区间合并（`consolidate_evidence_coverage`：同 path@revision 相邻/重叠窗口并集，不伪造覆盖）→ 仍不可表示则 UntrackedNovelty」；饱和不再并入 `equivalent_observation`。`update_convergence` 接收观察分类：未追踪观察计无推进债务但**跳过重复行为签名累计**；可证明推进清零 `untracked_observations`。`frontier_warning` 在未追踪期如实表达 "coverage untracked / cannot prove new or known"，不再说 "Re-reading known state"。

红→绿（实际生产 cap=`MAX_VISIBLE_BODY_WINDOWS`=16，不提额）：`saturated_ledger_reports_untracked_novelty_not_repeated`（红：报 RedundantEvidence）、`saturated_ledger_still_proves_known_windows_repeated`（正对照保持）、`bridgeable_window_consolidates_the_saturated_ledger`（红：假饱和；绿：并集 (1,5)+(6,10)→(1,10)）、`new_revision_resets_untracked_accounting`、`frontier_warning_reports_unknown_coverage_not_repetition`（红：提示含 Re-reading）。

**BR4**：`surface.rs` 新增显式 `TextOnlyFinalizationReason::{CompletionRepairTerminal, DecisionBudgetExhausted}`，`RoundSurfacePlan.text_only` 由两个 force 方法设置、`text_only_finalization()` 为唯一查询口；`model.rs` 门控收敛为纯函数 `refuses_for_unavailable_must(mode, unavailable_must) = mode.is_none() && !unavailable_must.is_empty()`——普通执行轮 fail-closed 不变，文本收尾轮豁免（不发工具、不加隐式轮次、不自动宣布完成，由模式文档约束）；文本轮无法满足的 MustSurface requirement 记入 omission（reason=Unavailable）审计，不再无声消失。

红→绿（`actor::model::budget_finalization_tests`）：`budget_text_finalization_round_survives_a_missing_required_tool`（红：临时还原旧门实测 round 2 `Unsatisfiable{Unavailable}`＋"refusing to start"＋TurnFailed、仅 1 次请求；绿：2 次请求、round 2 `Ready`、`requests[1].tools` 为空、报告含 `DecisionBudgetFinalization` 与 `Unavailable` omission、TurnCompleted）；`ordinary_execution_round_still_refuses_a_missing_required_tool`（负对照保持）；`naturally_converged_final_carries_no_budget_finalization_marker`；`only_ordinary_rounds_refuse_for_unavailable_must`；`surface::tests::text_only_finalization_modes_are_explicit_and_distinct`。

**验收**：`cargo test -p agent-runtime --lib` **446/0**（基线 436＋10）；`--test turn` 158/0 无回归；clippy/fmt 干净。

**限制**：`FrontierDelta` 是共享 contracts 枚举（不在本片所有权内），未追踪观察在 `ExecutionFrontier` 事件中呈现为 `NoProgress`（与 `RedundantEvidence` 可区分；专属 token 归 contracts 层统一收口）；预算强制 final 的标记依赖 round 内实际有 schema 被移置，turn 级 summary 字段属 contracts 层后续；从未保留的窗口无法彼此区分（同内容同参立即重读按内容身份报 Repeated，为"不保留即不可比"的固有边界）。

## C 线 — 测试控制器（BR5＋BR6＋BR7，scripts）——已关闭（`2bf7e1d6`）

**BR5**：runner 重构为 `RunnerConfig`＋`run_segment(cfg)`（可注入路径/env/子进程命令）；一处**无条件收尾**（停 relay 受理 → 限时优雅停 child → 仅终止自己创建的进程树 → reap → 记录确认状态）覆盖正常/异常/KeyboardInterrupt，metadata/summary/usage-ledger 终态必落盘；child 原始 exit code 逐字保留于回执。**退出码语义**（模块 docstring）：0 成功；2 CLI/环境非法；10 child 非零；11 保护文件被改；12 recovery fence；13 账目不完整（unknown/cap_stopped）；14 清理未确认；15 启动失败；16 身份不匹配（零付费受理）；17 中断；18 segment 已存在；19 child 超时由 runner 终止；20 内部错误；多类并发按优先序取一、回执 `categories` 列全。

**BR6**：campaign 级 `budget-ledger.json`（临时文件＋fsync＋`os.replace` 原子写）：`committed/reserved/unknown` 三类金额＋每 attempt 明细（跨进程单调 id、segment、request 序号、状态 reserved/committed/unknown/rejected_cap/upstream_rate_limited）。受理前在同一把锁内**预留保守上界**（body/4+256 tokens 按 miss 价 ＋ max_output_tokens 按输出价，标 `estimated: true`）；预留失败直接拒绝并标 cap_stopped；open 失败/断流/非 HTTP 异常在 handler `finally` 强制结算 unknown，收尾残留 reserved 一并强制 unknown；usage 严格按 `RESPONSES_USAGE_SCHEMA` 解析，缺字段记 unknown **不补零**；跨 segment 启动时读取账本沿剩余额度继续；CLI 数值有限非负校验（非法退出码 2）；实际 cap/预留策略/明细写入回执并标 estimated。

**BR7**：新 campaign 排他创建（已存在拒绝，提示 `--reset --yes` 或新 `--campaign-dir`）；`--reset` 是唯一重铺路径且先打印将被破坏内容的摘要（缺 `--yes` 退出码 4）；`l0` 不再重铺种子——先验身份（baseline-lock 存在、fixture/TASK/binary 哈希一致，不一致回执 `identity_mismatch` 列差异并退出非零）再跑 unittest/oracle。

**验证**：`python -m unittest discover -s scripts/tests -v` **25/25 OK**（三次连跑 16–18s；覆盖超时终止并 reap、KeyboardInterrupt 同收尾、启动失败、挂起 relay→unknown、正常终态、child 非零保留原始码、保护文件变化、第二次 setup 拒绝且哈希不变、缺字段不补零、并发预留互斥、跨进程账本继承、临界末次请求被拒、CLI 非法值）。红证据：实现前 25/25 全红（接口不存在/签名不符）。`py_compile` 通过。本机真实退出码抽样：child 非零→10（child_exit_code=7 保留）、超时终止→19（terminated_confirmed）、中断→17、保护变化→11、账目不完整→13、干净→0。**零真实供应商请求**（测试全程注入 env，未读 eval.env）。

**限制**：预留估计保守（按满额输出计），会提前拒请求但不低估账单；价格为常量配置（换 profile 需改 `Pricing`），仍非真实账单；收尾时仍在流式传输的 handler 晚于强制结算的用量按 unknown 保留（保守方向）；"清理未确认"分支（taskkill 失败且 kill 超时）无直接测试，代码路径存在且映射退出码 14。

## E-1 — 证据分层与 T8 标签（文档）——已关闭（`b78a3c94`）

- t8 walkthrough 标签修正：cross-run 首请求 hit=**1536**；16896 是 11 轮段总（原文把两者混为一谈）。
- 六项分层状态（自主交付／人工修复后验收／Runtime 故障覆盖／应用负载／KV 命中／费用对照）写入 NEXT_TASKS 第十二批 E-1 节；本批不改变 L1/FULL-PLAN 的 COMPLETE_WITH_MANUAL_REPAIR 分层事实。

## 第十二批集成回归（2026-09-19，本地 Windows，全部实际执行）

`cargo test -p context-simple` **464/0**；`-p agent-runtime --lib` **446/0**；`-p agent-runtime --test turn` **158/0**（1 ignored）；`-p agent-compose` 全套 **0 失败**（含 KV 序列 14.10s、proof supervision）；`-p agent-host` 全套 **0 失败**。`cargo fmt --all -- --check` 干净；`cargo clippy -p context-simple -p agent-runtime --all-targets -- -D warnings` 干净；`python -m unittest discover -s scripts/tests` 25/25；`python scripts/doc_consistency.py` OK。

**未执行**：真实供应商实验（本批不重跑付费 runner，预算/凭据条件不变）；.NET SDK 回归；GUI；Unix 平台语义。**下一步**（依 NEXT_ACTIONS 的 E-1 建议）：先定向反例与短同任务轨迹，应用负载实现相关变更才重跑 soak；KV 布局对照须同任务、同起点、同验收并含缓存读写/主/维护/重试总成本。
