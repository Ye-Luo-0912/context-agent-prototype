# 下一阶段可执行任务脚本：从基础设施转向可用 Coding Agent

> 状态：当前执行工单。代码核对基线：`12c86283b8d5991e9f17a07f14871dcf39d65066`。
> 下列 `/continue`、`/work`、`/plan`、`/review`、`--max-rounds` 中，基线尚未接入产品的入口均是**待实现功能**，不是现有命令说明。
> 本文替代旧文档的未来功能顺序，不替代 Core、Workspace、恢复、工具信封的安全契约，不修改历史评测结论。

## 执行总指令

你的任务是交付功能，不是再写一轮完整审计。

先读 CURRENT 与本任务，检查当前分支相对基线的相关文件差异。已经做完的内容做一次定向确认后跳过；未做完的内容只改该功能所需代码。不得因源码有新提交就重新启动全仓重审。

一次只做一个工单。D0 后首先交付 F1，不得一直停在 D0、测试或文档整理。功能完成以用户能做的新动作及代码实现为准；“新增测试通过”“补了设计说明”“影子统计有输出”不等于功能交付。

主线：

**继续/补充输入 → 简单任务清单 → 有界长任务与可取消验证 → 修改审阅 → 多文件上下文/搜索 → 三类真实任务走查。**

不引入 Chronicle 数据库、通用 Planner、TaskGraph、并行 worker、第二套 Runtime/TaskManager。不要先升级 GC、向量检索、学习排序或正式 Context Frame 编译器。

## 工单状态

| 工单 | 初始状态 | 前置依赖 |
|---|---|---|
| D0 文档切换 | 已完成（2026-09-06，`a28a5b9`：包应用 + README 链接 + LIVE_DOCS + 文档检查绿） | 无 |
| F1 继续执行与输入反馈 | 代码与定向测试已落地（2026-09-06，`c6fbbab`）；手工走查（恢复后 `/continue`、忙时排队处置）待做 | D0 |
| F2 实用任务清单 | 代码与定向测试已落地（2026-09-06）：`/work`、`/plan`、`TaskPlanView` 只读查询、恢复保留清单回归；手工走查待做 | F1 |
| F2 实用任务清单 | TODO | F1 |
| F3 有界长任务与可取消验证 | TODO | F1；与 F2 联调 |
| F4 修改审阅与交付 | TODO | F1；结合 F2/F3 |
| F5 多文件上下文与搜索 | TODO | 已有可操作产品入口 |
| F6 三类真实任务走查 | TODO | F1–F5 的所需路径 |

状态由执行者按真实结果更新。本文没有宣布任何待实现功能通过验收。每个工单完成后即可独立演示/合并；F6 不作为 F1–F5 交付前的新增总门禁。

---

## D0：切换文档入口，结束旧验收循环

### 目标

让 Coding Agent 一开始知道当前是交付功能，不再默认加载巨大审计文档，并执行已经过期的 M15 候选路线。

### 执行

1. 应用本包的 AGENTS、ROADMAP、CURRENT、STATUS、AUDIT_TODO、NEXT_TASKS 及两个旧 TODO 入口替换稿；先保留旧正文，再替换。应用脚本默认 dry-run，不触及功能代码。
2. 更新 `docs/state.json` 的当前队列和被核对源码。M15、LT-EVAL 的历史证据和 release 记录不改写。不要把旧 `ci.head_commit=green` 沿用成新源码通过。
3. README 不另加一套长路线：只将“当前开发方向/下一阶段”改成 CURRENT 与 ROADMAP 的短链接。保留安装、现有用法、许可证等内容。
4. `ARCHITECTURE.md`、`CONTEXT_LIFECYCLE.md`、`EXECUTION_COHERENCE.md`、`PLATFORM_SECURITY.md` 和现有恢复/兼容文档按模块需要查阅，不要求每项功能先通读。
5. `M15_ACCEPTANCE.md`、原始 `crates/agent-eval/evidence/` 和正式门禁代码保持原样。Frame-0/1/2 保留为可选测量；Frame-3 只约束将来正式翻转模型输入的研究，不约束 F1–F4。
6. 在现有 `scripts/doc_consistency.py` 的 LIVE_DOCS 加入本文件。只做这一类小范围链接/状态检查，不重建文档治理框架。
7. 检查 README 及仍被默认引用的入口，不再有把已关闭 M15、已发布版本当作下一项的文本。不要逐份清洗全部历史文档。

### 保留、移出与不做

| 文档 | 处理 |
|---|---|
| AGENTS / CURRENT / ROADMAP / NEXT_TASKS | 唯一活动入口与队列 |
| STATUS | 短索引，不再复制阶段历史 |
| AUDIT_TODO | 当前缺陷分流；旧原文留档 |
| LONG_TASK_EVALUATION / TOOL_ECOSYSTEM_TODO | 短参考入口；旧路线移出当前阅读路径 |
| 架构、安全、恢复、兼容、工具信封 | 保留现行契约；不以删安全约束换推进 |
| 历史报告与 evidence | 保留原始事实，默认不读，不生成自动工单 |

### 检查与结束

```bash
python scripts/doc_consistency.py
git diff --stat
git diff --check
```

文档任务不跑 Rust 全仓测试。检查通过、当前队列清楚，即提交/汇报 D0 并进入 F1。不要在这一阶段拆分十几份新文档。

---

## F1：真正接通“继续执行”和运行中补充输入

### 用户结果

保存/恢复或执行段停止后，用户可用 `/continue` 继续原任务；运行中补充不会静默丢掉；命令失败直接显示在 TUI。

### 已有基础

- `crates/agent-runtime/src/command.rs` 已有 `continue_active_task()` 和 `UserMessage`。
- `actor/turn.rs::start_turn` 在已有 turn 时进入 `queue_user_dialogue`。
- TUI 已有 notice channel、Runtime 事件、`/cancel`、`/suspend`、`/task`、`/restore`、`/checkpoints`。
- 当前 TUI 没有 `/continue`；busy 分支会拒绝普通输入；多种命令错误只写 tracing。

### 修改范围

优先 `crates/agent-tui/src/main.rs`、`state.rs`、`ui.rs`；仅在确需新的只读视图时改 RuntimeHandle。不要另写 continuation engine 或 UI 自己拼接历史。

### 执行

1. 增加 `/continue` 和帮助项，调用 `handle.continue_active_task()`。它不是 `user_message("继续")`，不得新造指令身份或重复 ingest 原始用户输入。
2. `/task <id>` 只激活任务，`/continue` 执行；恢复成功只显示“已恢复，可继续”，不暗中启动模型。
3. 忙时普通输入交给 Runtime 已有单槽队列。保留其已有排队/替换/拒绝语义，不加无界队列。界面分别显示“已提交/排队”“已应用”“拒绝或被替换”，依据实际 ACK/事件而不是 UI 猜测。
4. 不承诺队列消息能打断当前调用。按 Runtime 当前应用边界处理；用户要立即停止时使用 `/cancel`。取消结果须以 Runtime 的实际确认展示。
5. focus/task/suspend/pin/done/user_message 等命令错误走已有 notice 路径。异步操作不阻塞 UI 绘制，不向 alternate screen 直接打印。
6. 现有 ESC、Ctrl-C 语义明确显示；此工单不顺便引入新后台服务。

### 同批最小修复，不扩展成加固阶段

在接真实 release 使用之前处理两个已定位小错误；若当前分支已修复，记录后跳过。

**消费更新：** `crates/context-simple/src/engine.rs::acknowledge_consumption` 把 `stamp_consumed` 的实际调用移到 `debug_assert!` 外；原子性/所有权校验顺序保持。

```rust
let stamped = stamp_consumed(&mut state, *item_id, now_event_seq, turn, gc_epoch);
debug_assert!(stamped);
```

**重复选入：** materializer 普通候选轮必须跳过此前已选中的非 Pinned PromptRequired 条目。判重发生在扣预算前；不要只在输出尾部 dedup，也不要借机重写评分模型。

### 验收

- 手工恢复一个未完成任务，`/continue` 继续同一 TaskId 和原 directive，已有改动不重复执行。
- 忙时送一条补充能被排队并在既有边界应用；再送一条时处置可见，不默默丢失。
- 无任务、忙时 continue、非法 task id 等拒绝在 TUI 可见。
- release 消费更新与 debug 一致；一个 PromptRequired 条目只计入一次。

### 检查

先运行本次新增/相关用例，确认实际测试数不为零。可从下列命令定位，再选择已存在或本次新增的测试名：

```bash
cargo test -p context-simple -- --list
cargo test -p context-simple consumption --release
cargo test -p agent-tui
cargo check -p agent-tui
```

涉及 Runtime 行为时追加对应 continuation/queued-input 回归，而不是重复跑全工作区。合并使用现有 CI。

### 禁止扩展

不做新的 Checkpoint schema、不升级 GC、不重开 M15、不做全仓 debug_assert 搜索后的无限修复项目。

---

## F2：实用任务清单，不做 TaskGraph

### 用户结果

一个多步骤仓库任务有短清单；`/plan` 能看到已做、当前做、未做和阻塞。中断恢复后保留进度，模型可以调整执行顺序。

### 复用

`TaskAnchor.plan_progress`、`open_loops`、`next_action`，现有 `task.manage` CAS 提案、TaskManager 与检查点。工具已明确禁止模型通过进度字段修改用户目标、约束或验收权威。

### 执行

1. 增加明确的长任务入口 `/work <目标>`；普通短问答仍不强制列计划。沿现有 actor 的创建/激活与 user-message 路径开始任务，用户输入只应用一次。
2. 在这个任务上使用既有工具需求/lease 机制，使 `task.manage` 可用，优先 `PreferSurface`，不永久扩大全局工具表面，不授予额外权限。
3. 让模型用现有 `plan_progress` 维护通常 3–8 条清单；不是每个工具调用都写计划。首版使用普通字符串状态前缀即可，不新增 StepId、DAG、图调度器或新的计划日志。
4. 示例仅是显示约定，不是可信执行协议：

```json
{
  "base_anchor_revision": 7,
  "plan_progress": [
    "[x] 定位配置读取入口",
    "[-] 添加目标配置项及调用路径",
    "[ ] 运行相关检查并审阅改动"
  ],
  "open_loops": ["确认旧配置缺省行为不变"],
  "next_action": "修改配置解析并补一个相关回归"
}
```

5. 增加 `/plan`，从 Runtime 的有界任务视图读取这些字段。已有视图不足时新增一个窄的只读查询，不从 UI 私读 TaskManager，不为查看计划创建检查点。
6. 有界展示当前清单与一条 next_action。旧版本无前缀的 plan_progress 原样显示；读取行为不写状态。
7. 清单更新通过现有 `task.manage` 和 base revision；不让 UI 与模型各有一份权威计划。
8. `[x]` 只表示模型报告的进度，不是验证 PASS，不直接关闭任务。任务完成仍走既有 CompletionReadiness。

### 验收

一条两文件改动任务能产生清单、至少一次更新、`/plan` 查看；取消/保存/恢复后原清单和 TaskId 保留。过期 CAS 按既有拒绝反馈恢复，不丢 sibling 字段。

### 检查

仅补计划投影/查看、现有 CAS 更新与恢复用例；工具 schema 变化才补相应 conformance。优先复用现有 tool/task/turn 测试，不另建 checklist benchmark。

### 禁止扩展

不做每一步独立验收状态机、不强迫每一步一次 LLM 摘要、不建任务树、不自动把文本前缀解释为权限或恢复指令。

---

## F3：有界长任务与可取消验证进入产品

### 用户结果

用户能明确配置一个执行段的轮数预算；达到预算时显示可继续状态而非假完成。可信验证耗时时 `/cancel`、状态和补充输入仍能工作。

### 复用

`ComposeConfig.max_tool_rounds`、已有停止/continuation/安全点、`defer_proof_refresh`、`begin_deferred_proof_refresh` 和原 completion gate。

### 执行

1. 在 TUI 配置增加 `--max-rounds=<N>`，严格解析，传给现有 ComposeConfig。先核对 Runtime 实际计数单位，帮助与状态必须采用同一单位，不能把 tool calls 和 model rounds 混称。
2. 普通模式保持既有默认。长任务由用户显式指定较大的有限预算；正整数并遵守已有上限，不引入无限值，也不宣称某个默认数经过最优调参。
3. 当前回合到达预算后，复用既有结算和可恢复停止路径，显示停止原因、已完成改动、剩余清单、检查点和 `/continue`。无 durable checkpoint 时明确说不能保证冷恢复。
4. `/continue` 开始新的执行段；不要自动循环绕过用户预算，不把普通 final 当成“必须继续到清单全部打钩”。
5. 将已有 deferred proof 接入默认受支持 TUI 路径。先复用现有定向测试及一次真实慢验证，确认 turn-holding、取消与清理，之后才改变产品默认。
6. 重点核对：abort 一个 future 不等于子进程被杀死并回收。使用现有 ProcessSupervisor/取消/kill-then-reap 设施；迟到结果仍按 turn/generation/basis 拒绝。
7. 旧 inline 组合只在冻结实验/兼容需要时保留，不为了保持实验输入不变让普通产品永久停在旧实现。实验调用方显式保留原 flag。
8. 暂停/取消不自动清空 open_loops、失败或 effect debt；计划和限制应真实可见。

### 验收

一条超过普通短段预算的多文件任务能停下并手工继续；慢验证期间取消生效、子进程清理可确认、无迟到 PASS 完成另一任务；正常验证成功仍沿原完成事务结束。

### 检查

复用 `crates/agent-runtime/tests/turn/completion.rs` 的 deferred 测试及现有取消/continuation 用例，补实际产品 flag 与配置解析覆盖。不要新建完整故障矩阵或调度平台。

### 禁止扩展

不新增 daemon、后台排程、并行计划执行、token 预测器或第二个 completion coordinator。不抹除失败来换成“自动收敛”。

---

## F4：修改审阅与可理解的结果

### 用户结果

结束时能看到“改了什么、检查了什么、哪些未验证、剩余问题和如何继续”，而不是只有 completed/failed 或大量工具日志。

### 执行

1. 使用已有 git.status/git.diff、Workspace 变更记录、工件和 CompletionRecord，形成有界的当前任务结果卡。
2. 开始任务时记录已有工作区修改的范围/基线，复用现有工件保存必要的只读快照。不能 reset/stash 用户修改，也不能把整个 `git diff HEAD` 都归为 Agent。
3. 结果至少显示：改动文件与要点、实际检查及结果、未解决限制、任务是否持久完成/仅本轮结束/可继续。
4. 增加 `/review` 读取最近一份结果卡和 diff 工件。结果材料由已有工具回路/可信运行路径产生，UI 仅展示；没有材料就明确提示，不让 UI 私自调工具制造 authority。
5. 大 diff 保留工件与分页，TUI 只看摘要/选定片段。不新建 diff 编辑器、不做自动 rollback 全仓。
6. 非 Git 工作区也能显示已知修改文件；无法证明的归属或差异标为未知，不自动 git init。
7. `StatusProjection` 继续是读模型。只修本功能需要的“当前/历史”字段区别，不重建 Chronicle。

### 验收

在已有用户修改的工作区完成一个小功能，结果能区分原有修改和本次已知改动；实际失败或未跑的检查如实显示；`/review` 不触发新的模型调用或写入。

### 检查

一个干净工作区和一个已有修改工作区的短走查；一条有界结果卡/大 diff 工件显示回归。保留已有 completion、安全、output broker 契约。

---

## F5：多文件上下文与搜索能支撑真实编码

### 用户结果

模型读过多个片段后不会因“同文件同版本”误删互补内容；可以准确回看文件；搜索未扫描完时明确告知。

### 范围与顺序

先解决真实功能缺口，不做统一证据平台或新检索引擎。F1 的消费/判重修复只复核，不重复立项。

1. `fs.read` 保留整文件 revision 用于安全编辑 CAS；另外把实际返回区间/截断状态作为可信工具事实传给消费该信息的路径。
2. 去重先采用保守策略：只有已证明同一份内容/覆盖区间的正文才省略。旧记录缺区间、被截断或表示不明时按未知处理，宁可暂时多保留，也不声明全文可见。
3. 同版本的非重叠片段不得仅因路径相同被 supersede；不同版本的失效保持现有明确版本边界。不要先建立新 Evidence 数据库才能修这个问题。
4. 用户最新要求、TaskAnchor、当前清单和未解决错误优先，既有 GC 阈值不调。先利用现有工作集和显式 recall，不把全部历史重新塞回 prompt。
5. `search.grep` / `fs.list` 分清“结果分页还有内容”和“指定范围已完整扫描”。到文件数/字节/命中上限、跳过二进制或读取失败时提供有界覆盖说明。不能将局部无命中说成仓库无结果。
6. 文件读取尽量复用 fs.read 的受限句柄与字节上限路径；不把结果经 OutputBroker 裁短等同于工具中间内存有界。
7. 上下文 Error 的终结仅凭实体重合不充分。成功读文件/grep 不得永久清除错误；不能证明修复时保留或冷却，复用 Runtime 已有可信验证事实，不能新造第二 completion gate。
8. lease/TTL 一致性仅在当前用例暴露时定向处理，不扩大为全 GC 引擎改造。

### 验收

- 同文件 1–100 与 201–300 行在同一 revision 下互不冒充覆盖。
- 跨三文件做一次修改与复查；计划和未解决约束不因切文件丢失。
- 限制范围的 grep 结果能表述不完整，不伪造全仓无命中。
- 成功读取错误相关文件不会变成“错误已被验证修复”。

### 检查

相关工具/Context/Prompt 的定向回归加一条真实跨文件任务。字段需要跨 checkpoint/sidecar 传播时保守反序列化，保留旧格式的未知状态；不要为实验服务补齐所有新特性后才发布默认进程内路径。

### 禁止扩展

本轮不接向量库、BM25 新服务、SIEVE/TinyLFU、语义路由器、全局知识图谱，不启动 Frame-3 正式输入切换。它们属于之后单独选择的研究题。

---

## F6：用日常任务决定下一项，不再扩建评测框架

### 执行

用同一实际产品配置、实际 TUI 和隔离的可丢弃工作区，分别完成：

| 任务 | 需要看见的功能 |
|---|---|
| 修复一个真实小 bug | 定位、修改、相关检查、结果审阅 |
| 增加一个小功能 | 清单、两到三文件修改、限制不漂移 |
| 一次跨文件重构并中断继续 | 保留计划、恢复同一任务、不重复副作用、回看正确片段 |

不要为了运行它们新建 pack/schema/oracle 系统。复用现有日志、diff 和工件，每项记录用户目标、源码与产品配置、实际结果、检查、未解决限制。

一次普通失败先判断是模型、工具、上下文、配置还是产品接线。修直接阻塞这条功能的错误；没有证据不要继续增加安全矩阵和统计门。
真实 provider 不可用就记 NOT_RUN，保留已经完成的确定性功能成果；不假装 live 通过，也不因此倒退去重做全部基础设施。

### 完成标准

三类流程各有一份可检查的真实记录，失败透明且主要产品入口可操作。它们是产品走查，不是模型成功率/算法优越性的统计证明，不替代既有安全回归。

下一项从真实失败或用户需求选择。优先候选是复用现有 Compose/RuntimeHandle 的非交互入口，然后才是编辑器接入；不自动跳到多 Agent。

---

## 防止再次陷入测试/文档循环

- 每个功能先给出一个具体演示动作；没有功能行为变化就不标 F1–F6 完成。
- 修改时跑定向检查；PR/集成沿用现有 CI。相同代码、相同命令没有新原因不反复重跑。
- 已经关闭的切片先复用，不换名字再做。
- 无关边角问题一行记录，不能追加进本工单的结束条件。
- 同一工单连续两轮没有可演示推进，应报告具体阻塞与更小替代实现，不继续扩大框架。
- 安全与数据完整性不能靠“快点落地”绕过；真问题修当前路径，而不是宣布全仓永远必须先清零。
- 不用修改 frozen tests/oracle、跳过失败、提高所有预算来制造进展。

## 每个工单的交付回执

```text
工单：
新增的用户动作：
复用的现有实现：
实际修改文件：
执行命令与实际结果（含未跑）：
演示结果：
仍有的限制/本次延期项：
下一工单：
```

回执保持短。旧审计、原始日志和大 diff 放在现有工件/历史位置，不复制回 CURRENT 或 AGENTS。
