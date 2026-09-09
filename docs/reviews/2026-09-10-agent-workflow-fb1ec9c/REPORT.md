# 2026-09-10：从 Agent 任务流程审查核心

基线：`fb1ec9c069fea05911fa360408c5b117fad90219`。本轮确认 **8 项流程问题，4 个 P1、4 个 P2**。每项均由主审查者亲自复核调用链并运行隔离反例；证据层级有差别，详见 [EVIDENCE.md](EVIDENCE.md)。未修改生产代码、提交或重跑全仓/冻结评测，也未连接真实 provider。

上一轮 R01–R14 已有修复提交。本轮定向确认了冻结欠账交接、schema 编译前移、pending owner、recovery roots、范围覆盖和 Rolling 分段消费等实现，重点检查这些组件组合后，Agent 能否保持任务意图、拿到证据、完成验证并及时让出控制。网络安全未深入。开始时沿用既有两条并行分工；用户要求减少子 Agent 后立即停止子任务，此后源码核对、全部反例和报告均由主审查者完成，未新开代理或切换轻量模型。

这里的 W 编号只是本报告定位，不另建阶段或替代 `NEXT_TASKS.md`。后续顺序、任务流程和衡量方式见 [WORKFLOW_AND_ROUTE.md](WORKFLOW_AND_ROUTE.md)。

## W01 / P1：继续任务时，完整指令被预览替代

**Agent 的处境：**第一段执行收到完整要求，继续时却看不到要求尾部。模型无法知道自己少收了一段约束，仍沿同一 directive basis 工作。

`agent-runtime/src/task.rs:1387–1399` 的 `on_user_turn` 将 `turn_intent` 截为 `MAX_TASK_ANCHOR_TEXT_CHARS`，即 2,000 字符。`actor/turn.rs:399–415` 用这个字段构造 TaskContinuation。继续不重新 ingest 原正文（`:270–292`），而 Rolling 的 `shared.rs:184–188,197–214` 又按 created_turn 排除“当前用户输入”，认为 TurnFrame 已经带着它。因此原文即使仍在 Context，也不会自动补回继续请求。

实际公共 RuntimeHandle 探针：建立小目标，提交 2,100 字符前缀加尾部唯一约束；脚本模型结束执行段，等待 TurnCompleted 后调用 `continue_active_task`。两次完整 ModelRequest.messages 中，尾部约束的可见性为 **[true, false]**；继续成功，Context checkpoint 仍含原文。使用 Rolling、真实本地工件工作区和实际 Actor；没有调用真实 provider，也没有验证真实模型是否因此做错修改。

应满足：同一当前指令身份的继续，必须保有同一完整正文或可验证、可读取的正文引用；`preview(directive)` 不能充当 `directive`。修复应沿用 RuntimeInputEnvelope 的引用/digest，在 TaskRecord 保留当前指令身份；2,000 字符只用于展示。必要回归检查整个最终模型请求及正文引用，而不只检查 TaskId 和 directive_revision 没变。

## W02 / P1：最终预算删掉必需正文，却不记缺失

R09 已修 prompt 与 materializer 的区间覆盖；但 `actor/model.rs:28–83` 的 `final_frame_body_key` / `record_final_pack_drop` 仍仅比较 `path@revision`。最终 packing 在 `:951–999` 删除正文后调用它；只要剩下一份同版本文件的其他区间，就认为原正文还可见。

反例含两份都必需的正文：较大的 L1–100 与较小的 L101–200。直接编译当前生产 helper，执行真实的最大项选择、移除及缺失记录逻辑；L1–100 被删除后，**required_body_present=false、required_misses=0**。这是 final packing helper 级探针，不是完整 provider 预算端到端测试。

后果不仅是少看内容：`:1085` 仅在 required_misses 非空时撤回 settlement candidate；Actor completion safety 也消费该缺失观察。缺失证据可能未成为完成阻塞。默认 OperatorClosureOnly 仍有效，本轮没有证明绕过操作员的完成权限。

不变量：`required_body 被移除且没有同请求覆盖副本 ⇒ required_miss`。最终 packing 应复用 R09 的范围包含判定，包含同 ID 的部分副本、不同 ID 的互补区间和未知范围；不能到最后阶段退回较弱的字符串身份规则。

## W03 / P1：旧快照根只保护 reconcile，没有保护正常 Storage GC

`actor/restore.rs:225–230` 已把恢复根传给 `reconcile_store_protecting`。但任务完成会在 `actor/turn.rs:2291–2292` 调用 `run_storage_gc_at_boundary`；`actor/lifecycle.rs:381–385` → `services.context_storage_gc` → `context-simple/store.rs:1029` 的删除计划仍没有 checkpoint recovery roots。

实际 Context API 反例：真实外置 Live 正文，保存可读取它的 checkpoint A；构造后来已终结且超过 TTL 的当前状态；提取 A 的 recovery roots，protected reconcile 正确保留 blob；随后普通 Storage GC 删除 1 个 blob。恢复 A 后，其 Live 正文 **fetch=None**。当前终结/老化状态通过合法 checkpoint 夹具设置，文件 IO、根提取、reconcile、Storage GC 和 restore/fetch 都是真实引擎调用；未做完整宿主任务完成及冷重启实验。

`Delete ∩ Reach_strong(CurrentRoots ∪ RetainedCheckpointRoots) = ∅` 必须约束每个物理删除入口，而不只是恢复时的清理。应在既有 Storage GC 删除准入处统一使用根集合，保持工作集迁移与物理删除的区别。

补充静态风险，尚未故障注入：`collect_checkpoint_recovery_roots`（`restore.rs:261–280`）将 list 错误变为空集合、将 load 错误跳过；根枚举不完整不能证明没有保留者。后续修复需要携带“根集合是否完整”，未知时暂缓删除，不把读失败包装成空集。

## W04 / P1：一次上下文维护可执行大量模型调用，并拖住 Actor 取消

R08 的分段消费保存了未读残余，但 `context-baselines/rolling.rs:424–473` 会在一次 maintain 中循环，直到阈值满足。每段 `source ≤ 2,000`、`output ≤ 512`，不代表整次维护的调用数、总耗时或费用有界。`agent-compose/src/compactor.rs:32–51` 直接 await 模型，并创建与当前操作无关联的新 CancellationToken。

两项独立反例：

- 默认 RollingConfig，一条 200,000 字符旧输入加近期消息，一次 BeforeModel maintain 产生 **132 次串行 compactor 调用**。这是脚本压缩器调用次数；66,000 输入/16,896 输出 token 是脚本设定 usage 的累计，不是真实 provider 消费。
- 公共 RuntimeHandle + 带屏障的 compactor：维护已进入等待后发送 cancel_turn；250 ms 内没有取消回执。释放维护屏障后才返回 Cancelled，随后 stop 成功并确认 Actor 任务结束。这验证了等待依赖关系，不是生产延迟基准。

源码原因：`actor/model.rs:208–211` 等入口在 Actor 命令/完成处理分支内 await maintain；`actor/mod.rs:1578–1587` 直到该分支返回才再次处理命令。单个 provider 的超时不能给 132 次串行请求提供任务级 deadline。`--max-rounds` 计量决策轮，也不能替代维护预算。

修复应保留 Actor 唯一编排，给维护明确的调用数/字符量/时间预算和取消身份，超预算保留未处理残余并报告延期。必要的长等待通过既有异步操作完成消息交回 Actor，在代次校验后提交；不新增并行 worker 或通用调度器。验收须覆盖“维护已开始时取消”，以及主决策之外的总模型调用预算。

## W05 / P2：合法验收集合超过当前证据容量，repair 无法收敛

契约允许 16 个 verification coverage domains、32 条 acceptance criteria（`agent-contracts/src/tool.rs:465,2346`）。`ExecutionState` 却只保留最近 8 个 VerificationFact（`execution/state.rs:17,2189–2191`），而 `task.rs:984–997,1183–1216` 要求验收 receipt 引用的 PASS 仍在这个数组里。

实际公共 ExecutionState 探针在**同一 directive/workspace/spec basis** 上记录 9 个不同域的可信 PASS。结果 validity=Current，但只有域 1–8 的 PASS 可查询；补跑域 0 后，当前集合变成 0、2–8，域 1 又失去证据。探针构造与当前测试一致的 host attribution，不执行真实验证进程；验收 gate 的依赖关系由源码核对。

对一个合法、要求这 9 个独立域的任务，`|required_domains|=9 > |retained_PASS|=8`，因此至少一项始终 uncovered。不是让模型多试几次就能解决。该问题针对显式配置的多域自动验收；默认 OperatorClosureOnly 不因此获得自动完成权。

修复方向是把当前验收所依赖的证明与一般历史尾部区分保留，仍由现有 ExecutionState/TaskAnchor 管理。仅将 8 增大而继续按调用次数淘汰，仍可能被同域重复验证挤掉其他域。必要回归：9 域全部通过、重复其中一域、进度更新后，其余当前域证明仍有效；世界/指令/检查定义改变时仍应按规则失效。

## W06 / P2：大日志的按行恢复入口不可达

进程日志允许捕获 8 MiB（`tool-runtime/src/tools/stream.rs:27`），但 `artifact.read` 在使用 start/end line 前，先读取至 2 MiB+1 并拒绝（`tools/artifact.rs:100–119`）。错误建议“use a narrower range”，缩小范围却不会改变此前判断。

真实 Workspace + BuiltinToolDispatcher 探针写入合法的 3,000,000 字节工件，分别请求前 200 行和第 1 行，两次均被同一大小检查拒绝。没有运行进程来制造该日志；生产者上限由源码确认。平台的其他工件读取接口不等于模型已有可用工具路径。

需要让模型对合法产出的工件拥有有限步、受预算的恢复路径。按范围有界读取，或提供实际可达的 chunk/tail/search 操作，返回准确游标与不完整原因。拒绝建议必须有改变结果的可能；保留每次 IO/输出预算，不直接无限读取或增大全文上限。

## W07 / P2：patch 拒绝的“当前内容”来自未提交中间状态

`tools/patch.rs:398–450` 顺序在临时 updated 上应用 hunk；后续 hunk 失败时，把 updated 和磁盘旧 revision 一起传入 `patch_refusal`。`:638–660` 将它渲染成纠错候选并称 current revision，却没有声明它是部分 patch 的假设结果。

真实 dispatcher 探针：第一 hunk 把 old_value 改为 NEVER_COMMITTED_VALUE，第二 hunk 因后续上下文不匹配失败。返回 ok=false，磁盘完全未变；候选却包含 **NEVER_COMMITTED_VALUE** 并带旧 revision。探针提供结构化 effect recovery identity，未绕过 Core 执行提交，也未进入 staging/commit。

Agent 可能据此生成下一次 patch，反复引用磁盘上不存在的文本。应满足 `current_candidate(revision) ⊆ actual_content(revision)`；若保留中间 hunk 诊断，须明确区分 hypothetical、给出失败 hunk index，并另提供真实磁盘基准。默认纠错候选应取 original。

## W08 / P2：真实压缩适配器把模型失败包装成可提交摘要

Rolling 在 BoundedCompactor 返回 Err 时会归还记录；但真实 `ModelBackedCompactor` 的 `compactor.rs:53–61` 把任意模型错误改成 `Ok(fallback)`。fallback 仅保留 512 字符前缀；Rolling 因而按成功提交并移除原记录。现有适配器测试只检查短输入能够 fallback，没有验证长输入的源保留。

探针直接编译当前适配器源文件，注入必定返回临时 Model 错误的脚本 transport。旧记录的唯一约束位于 700 字符之后、实际请求源之内：模型调用失败，但 maintain 报 archived=1，随后 engine checkpoint 不再含该约束。没有真实网络调用；原始历史可能另有工件保存，因此未声称全系统永久删除了用户正文。

应区分 `summary_completed` 与 `summary_unavailable`。临时错误、取消和未知结果应交给引擎保留/延后消费，或明确保留可恢复残余，不能因显示用 fallback 非空而获得退役源正文的资格。这也是 W04 维护取消落地时必须同时守住的失败语义。

## 覆盖与交付边界

本轮深读输入/继续、TaskAnchor/验收 receipt、ExecutionState freshness/证据上限、prompt 最终 packing、维护/恢复/完成边界、Rolling/live compactor、Storage GC 与部分工具反馈路径。平台只核对相关入口和默认 context policy；未重审 GUI，也未声称全仓逐行完成。

本轮仍有未升级为确认问题的候选：恢复根枚举失败、Rolling partial 失败时统计回退、完成任务累计达到 checkpoint 大小界。它们不增加主表数量，不启动算法研究。证据完整性的具体修复与平台功能可按原所有权并行推进，不能用“全仓审计清零”阻塞现有主线。

检查命令、输出、夹具修正和限制见 [EVIDENCE.md](EVIDENCE.md)。当前报告是审查与路线建议，不是实现完成回执。
