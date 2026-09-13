# M18 第三轮 A 线 CTX-10 / CTX-2 残余 / CTX-11 / CTX-12 实施回执（2026-09-13，工作树）

对应第三轮审查 R3-02/03/04/05/06/07 的四个切片，A 线全部代码落地。每片先红检查（证明测试捕获旧行为）后转绿；共新增回归 **6 项**。**未提交/未推送、未跑远端 CI；真实 provider 照旧 NOT_RUN。**

## 实际执行的验证

| 目标 | 结果 |
|---|---|
| `cargo test -p context-simple --lib` | **372/372**（含本轮 6 项新增；含复跑 CTX-10 的退休后读回边界） |
| `cargo test -p context-baselines` | **25/25**（含并行 COST-8 新增 2 项） |
| `cargo fmt` / `clippy --all-targets`（context-simple） | 0 diff / **0 警告** |
| `python scripts/doc_consistency.py` | OK（13 live docs） |

共享树并行域（如实记录，不归属本线）：并行会话将 `ContextEngine::checkpoint_recovery_item_ids`/`reconcile_store_protecting` 改为 async（EXEC-9 在飞），A 线对 `tests/residency.rs` 两处调用做了机械 `.await` 适配；agent-runtime/agent-eval 的在飞契约收口仍在并行域。

## CTX-10（R3-02 P1 ＋ R3-04 P2）：scope 退休后仍可召回和恢复

1. **释放事实胜出（R3-02）**：`store::reattach_owner_metadata` 的 scope_id 合并从「entry None 且 blob Some 时保留 blob」改为**entry 无条件胜出**。entry 的 None 现在是当前 owner 的*显式释放*（CTX-9 的 close/外置释放路径），blob 的旧印章不得把退休 scope 当悬空引用带回工作集；pre-scope 时代旧行由此降级为 task_id 的 legacy 推断——全引擎已视为一等公民的路径，绝不产生悬空引用。退休不变式保证 entry.scope_id=Some ⇒ scope 必在树中（退休的引用检查含全部 entry）。
2. **退休环按时间序（R3-04）**：新增事实追加在既有事实之后、满员淘汰最旧前端——最新完成事实绝第一个被淘汰的旧缺陷消除；首次淘汰时置 `State.retirement_ring_overflowed`（serde default）。
3. **保守完成语义（R3-04 后半）**：新增 `scope::completion_facts(state) -> CompletionFacts`（certain 完成 facts＋树内 Task scope 集＋conservative 标志）。环一旦溢出，「无 live scope 且无注记」的未知任务保守视为已完成——其留存的正文不会仅因有界窗口遗忘而重新获得自动 hot 召回（显式理由召回不受影响；活动任务的 scope 必在树中，永不误伤）。`completed_task_facts` 保持单一事实来源，GC sweep/mark/reactivate/commit 共享同一快照。
4. **回归（2 项，红检查均在先）**：`a_retired_scope_is_not_brought_back_by_its_blob`（外置→close→退休→ResidentRequired 召回→scope 引用合法→**引擎自身 checkpoint 可 restore**；红：召回带回退休 scope 且 restore 报 references missing scope）；`retirement_ring_keeps_newest_facts_and_completion_stays_conservative`（513 次真实工具 scope 开闭逐次 GC 溢出环→最新事实保留、古老任务正文不被自动召回；红：环永不溢出/事实被丢）。

## CTX-2 残余（R3-03 P1）：撤销需要完整宾语证明

`names_replaced_object` 的证明判据从「替换宾语与旧决策共享**任一**内容词」收窄为「替换宾语的**每一个**内容词（双侧排除停用词与文件路径词）都出现在旧决策中」。反例「replace timeout logging in AuthService.rs with structured events」与「5-second timeout」只共享维度词 timeout，不再授予不可逆 Superseded——不确定即 Live 共存，相关性调整留在检索/注意力层。路径词双侧排除（沿用该函数既有文档语义）使带位置状语的合法撤销（"replace the 5-second timeout **in AuthService.rs** with a 30-second timeout"）照常成立。未新增任何关键词。**回归 2 项**（红检查在先）：`modifying_timeout_logging_does_not_withdraw_the_timeout_requirement`（同一决策四正文位置 Resident/Warm/Pending/Stored 同判定，全部保持 Live）；`naming_the_full_requirement_still_supersedes`（全宾语点名的明确撤销照常生效）。既有 F02/CTX-2 全部 35+33 项回归保持绿。

## CTX-11（R3-05 ＋ R3-06 P2）：材料化复用四 owner 与当前元数据

1. **R3-05**：`plan_foreground` 在 heap/buffer 之后、stored 之前接入 Pending（外置重试列表）查找——store 故障期间当前编辑文件不再 Missing（fetch 一直可服务同一正文）。
2. **R3-06**：`ForegroundPlanItem::Store` 与 required 的 Store 计划一致，携带 plan 时的 entry owner 快照，`realize_foreground` 读回后经同一 `reattach_owner_metadata` 合并——最终 `MaterializedContext.foreground` 携带当前保留/作用域事实（如提升后的 Durable＋Promoted），blob 只负责内容与创建身份。至此四个 blob 读回路径（fetch/admit/GC recall/required）＋foreground 全部复用同一合并规则；选择优先级、预算、版本/范围校验保持，投影不变成 Admit。
3. **回归 1 项**：`foreground_projection_agrees_across_all_four_owner_locations`（Pending 文件的 foreground 投影＋fetch 一致；Stored 条目 owner 保留调整后 foreground/fetch 均携带当前值。红检查两腿分别验证：撤 Pending 臂→Missing；撤合并→帧带旧 Working）。

## CTX-12（R3-07 P2）：召回额度只在成功召回处扣

warm 扫描循环重排：先由 `reactivation_reason` 判定，命中后才消耗 `remaining`（权威 anchor 召回仍走独立上限，不含于该额度）。无效候选（如不可自动召回的 shell observation）不再花掉数量预算饿死其后的有效候选；扫描工作仍受既有批次/游标边界约束，未新增扫描上限。打分、默认阈值与 anchor 权限语义未动。**回归 1 项**（红检查在先：预算 1 时无效候选先行 → reactivated=0）：`invalid_warm_candidates_do_not_consume_the_reactivation_budget`（budget=1，无效候选最新在前，有效 note 首趟即召回）。

## 语义边界保持

GC 语义终态、`visible_body_windows_cover` 唯一省略依据、冻结打分/阈值（active/archive/gc_max_generation 未动）、TaskManager 权威（A 线未改 Runtime）、ContextEngine 可替换性（baselines 25 绿）全部保持。契约仅 State 内部布尔（retirement_ring_overflowed，serde default，引擎内部序列化），无 wire 变更。
