# 后端长流程与 TUI 续审

基线：`d92564bcfa41dda44f752e1e49e88a321abdf942`  
审查日期：2026-09-16  
远端 CI：run `35036238204`，success，attempt 1。  
[固定提交](https://github.com/Ye-Luo-0912/context-agent-prototype/commit/d92564bcfa41dda44f752e1e49e88a321abdf942) · [对应 CI](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/35036238204)

## 结论与范围

继续现有后端阶段，不新建架构、不恢复 GUI 扩展。TUI 已经是同进程正式 Runtime 的操作入口，应优先保证它能完整展示待审批动作、正确呈现当前任务、可靠重放事件、持续接受控制，而不是先做外观重构。

本轮全文读取 `agent-tui/src` 的八个源文件（含其内联测试）、`agent-tui/tests/real_binary_startup.rs` 和该 crate 的 Cargo.toml；另外对 Runtime、Context、Provider 和服务适配做固定版本的调用链续审。**这不是全仓全部文件逐行审查完成**，其他部分的精确读取窗口见 [COVERAGE.md](COVERAGE.md)。

环境中的 git 访问仍报 `Could not resolve host: github.com`，没有可用 cargo、rustc、dotnet。下列是源码确认的控制流、库文档核对和带条件的反例推导；没有在本环境运行 Rust/.NET 回归、真实终端/PTY 或付费模型实验。远端 CI 成功不能代替本轮新增反例已验证。

## 本轮保留的已有进展

- 搜索在 SimpleContextEngine 内已原子返回 hits、coverage、observation，服务 adapter 已在协商成功时传递同一报告与 continuation；未协商时明确拒绝，不再默认自证完整。
- 缓存断点已收敛到一个内容块放置函数，工具结果的字符串转成 input_text，不再沿用旧 output_text/sibling fallback。
- 取消用量已有 Compose→Retry→Cancel 的正式事件结算回归；这次读取了该回归，未在本机执行。不要把旧“只能靠可选 metrics 文件保留”的问题照原样重开。
- `work.rs` 仍是共享 start_work 的薄适配器；TUI 不需要自己再实现一套任务创建/执行权威。

来源：[crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-simple/src/engine.rs)；[crates/context-contextcore/src/adapter.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-contextcore/src/adapter.rs)；[crates/provider-openai/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/provider-openai/src/lib.rs)；[crates/agent-compose/tests/cancel_usage_settlement.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-compose/tests/cancel_usage_settlement.rs)；[crates/agent-tui/src/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/work.rs)。

## 发现总表

优先级是本次派工建议，不是 CVSS，也不表示所有条件在默认运行中均会触发。除注明的库语义核对外，新增反例均尚未执行仓库回归。

| ID | 建议 | 问题 | 影响边界 |
|---|---|---|---|
| U1 | P1 | 审批正文不可完整查看 | 操作员无法充分核对长参数；不是 Core 权限绕过 |
| U2 | P2 | 多行消息被构造成单行，滚动与字符宽度口径不一致 | 终端显示，不是原始模型/工件文本被删除 |
| U3 | P2 | 实时状态与重放状态分裂，水位只保护部分字段 | 状态回跳、重复计数和对话身份混淆 |
| U4 | P2 | review 卡片混合任务，容量满后静默丢弃后续项 | 展示/复核错误，不是 Runtime 真正完成了错误任务 |
| U5 | P2 | 终端恢复只在正常控制路径，且晚于异步 shutdown | 早退或异常时遗留 raw/alternate 状态 |
| U6 | P2 | 独立 spawn 命令不保证键入顺序；慢控制命令阻塞输入循环 | 条件性的任务目标错配和失去交互能力 |
| U7 | P2 | headless 事件缺口没有进入成功/完整性判定 | 丢失失败事件后可能仍返回成功；Closed 另列防御边界 |
| B1 | P2 | 批量 required 冷解析互相驱逐 | 已经读取成功的目标被再次报为 Missing |
| B2 | P2 | 存在的卡片路径被当作内容验证证明 | 存储损坏条件下，新 checkpoint 可能引用坏元数据 |
| B3 | P2/后续 | 生命周期 ledger 导出先取走记录，再 await | 取消导出会丢内存日志；不涉及 Core authority WAL |

## U1：审批应当能够核对完整动作，而不只是显示工具名

`ui.rs` 最后一块高度固定为 3，四周边框占用上下两行，只剩一行正文。审批 Paragraph 却包含工具说明、参数和确认提示，且未提供审批内容的滚动布局。`state::begin_approval` 保存的总参数预览只取前 220 个字符；对话中的参数日志最多八项，每项取前 120 个字符。后者并未标记每一项的尾部截断。`session.rs` 在 pending approval 时只处理允许/拒绝，PageUp 等导航被忽略。

不能说“完全看不到任何参数”：对话日志确实有参数前缀。但长命令尾部、较长路径、文件替换正文可能没有完整的 UI 查看路径，且审批中无法翻阅旧行。这是知情审批的缺口，不是样式偏好。

**最小修复**：保留完整、受 Core 已有请求上限约束的审批数据或一个可信的完整查看引用；可滚动的详情面板；明确列出截断/省略；确认控件保持可见。缺少完整详情时明确不可核对，不应伪装为充分展示。确认必须绑定实际 request_id，过期请求不能批准新的动作。无需强制用户逐字符阅读，也不要仅为了把所有文本塞进屏幕而加大高度。

**反例**：同一长参数的前 120 字符相同，关键目标不同且只在尾部出现；在 80×24 和窄终端上经真实 render + 按键翻页能核对尾部 sentinel；大量参数必须可继续查看。当前 state 单测只检查内存消息含字段，不能证明屏幕可见。

来源：[crates/agent-tui/src/ui.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/ui.rs)；[crates/agent-tui/src/state.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/state.rs)；[crates/agent-tui/src/session.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/session.rs)。

## U2：多行显示、换行计数与输入光标需要共用布局语义

`conversation_lines` 将一条消息正文整体交给 `Line::from(message.content.clone())`；Context 面板也把带换行的内容装进单条 Line。Ratatui 0.30 的 Line 构造/转换语义会去掉换行，它不是多行 Text。因此代码块、错误栈、计划和帮助文本会失去原始行结构，后续 Wrap 无法恢复原来的换行。

当前自动滚动以总行宽除以宽度估计折行数，但 Paragraph 的单词换行与这个估算不一致。输入光标用 chars().count() 而非终端列宽，中文、宽字符和组合字符的光标位置也会错；长输入没有配套的横向视窗。这些属于同一布局切片。

**最小修复**：保留多行 Text 或显式生成多条 Line；以同一布局规则计算视窗和滚动边界；用终端显示列而非 Unicode 标量数量计算光标；处理尾部/空行/窄窗。没有必要同时实现复杂 Markdown 编辑器。

**反例**：两行正文必须出现在不同 buffer 行；代码缩进、空行、末行 sentinel 保留；词组换行后尾部可见；中文长输入光标始终落在输入区域内；resize 后滚动不回跳。用真实 Ratatui TestBackend/Buffer 断言，不能只对 conversation_lines 输出字符串取子集。

来源：[crates/agent-tui/src/ui.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/ui.rs)；[Ratatui 0.30 Line 文档](https://docs.rs/ratatui/0.30.0/ratatui/text/struct.Line.html)。这是显示层问题，不要将它误写为“模型上下文原文被压缩丢失”。

## U3：重放后必须得到同一份视图，而不是只修一份计数器

AppState 维护 StatusProjection，同时独立维护 status/busy/current_model_operation/current_task、局部 tokens、review 卡片等。`resync_projection` 只重建 StatusProjection 并按文字补部分对话；没有等价重建这些其他字段。`apply_runtime_event` 对小于等于 replay watermark 的事件只跳过 projection.fold，仍继续后面的本地字段 mutation。

因而“重放到较新终态，再从 broadcast 读到旧 ModelStarted/ModelUsed”可以重新激活旧操作或重复计数。相同文本的跨回合消息又以字符串内容去重，合法的第二次同文回复可能消失。内容不是事件身份，用户输入的同 ID lifecycle 更新也不能与不同 ID 同文输入混为一谈。

StatusProjection 本身还未折叠 TurnCancelled 等若干终态；不能简单把所有小部件改成读它就结束。日志扫描的错误和不完整也需要保留：解析失败被跳过、文件集合被截断或目录读取失败，不应仍报告完整并把最大 seq 当作连续水位。

**最小修复**：一个共享、带身份的 reducer；区分可重放持久事件与只用于当前操作的 live delta；同一 (RunId, seq) 最多应用一次，流式 bubble 绑定 TurnId/OperationId/generation。用已验证 snapshot + 后续序列重建；未覆盖区间维持 Unknown/Partial，不能用最大序号跨越未验证缺口。只把输入草稿、选中页、滚动位置留在本地 ViewState。

**反例**：正常逐条实时消费得到状态 A；另一实例先遗漏一段、重放，再重复投递已覆盖 ModelUsed/TurnCompleted/ModelStarted，得到状态必须等价。增加取消无 usage、相同文字不同 turn、坏 JSON 中间行、日志覆盖不全场景。

**测试维护性**：现有去重回归重复的是 RunStarted，它本来就会将 bool 设为 true；即使去重失效也可能通过。应使用真正会累计计数或改变当前操作的事件检验。

来源：[crates/agent-tui/src/state.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/state.rs)；[crates/agent-runtime/src/status.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-runtime/src/status.rs)；[crates/agent-tui/src/session.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/session.rs)。

## U4：review 应按任务归属，容量限制也必须保留遗漏事实

ResultCard 的 changes/checks 没有各自的 TaskId。任务 A 完成后保存 completion；开始任务 B 时未清空/切换这张卡片，B 的工具结果继续追加。界面可能在 A 的 durable completion 标题下列出 B 的修改。其文字说明 changes 是“本会话”，但这不能解决它与单任务完成头混用的问题。

卡片满后只保留最早的 32 个检查/文件等条目；后续项不追加，也没有独立的 omitted counter。format_result_lines 再用 len-cap 计算遗漏永远看不见那些已被拒绝追加的项。晚到的失败检查可能不出现在卡片中。

卡片导出还通过独立 task 写同一个 result-card-latest.json，没有序列化更新/临时文件提交；这是 advisory 产物，不能作为运行权威，但也不应旧快照覆盖新快照或半写被读成无结果。

**最小修复**：按 Run/Task 及实际工具事实归属维护或查询卡片，明确会话卡与任务卡的不同。完成头只适用于对应任务。显示限额与总数/遗漏计数分开；保留最近检查并优先保留未解决失败，未保留部分可继续读取。导出用单写者/有版本的原子快照，加载验证身份并有界读取。工具调用成功不是测试 PASS，计划 [x] 也不是证明。

**反例**：A 完成→B 修改并校验失败→/review B，不得显示 A 的完成头；第 33 次检查失败不能静默消失；相同路径跨任务不互相覆盖；双次快照乱序完成时较新版本不被覆盖。

来源：[crates/agent-tui/src/state.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/state.rs)；[crates/agent-tui/src/session.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/session.rs)。这不是声称 Runtime 已将 B 错误地持久化为 Completed。

## U5：终端会话的恢复责任应独立于 Runtime shutdown

main 依次 enable_raw_mode、EnterAlternateScreen、Terminal::new、clear，然后运行 session。任一步在 raw mode 开启后通过 ? 返回，都可以绕过正常尾部 cleanup。正常结束也先 await composed.shutdown，再 disable raw / leave alternate / show cursor。手工 Terminal::new 不等价于安装 panic hook。

**最小修复**：小型 TerminalSession guard，跟踪已成功启用的终端状态并在早退时恢复；安装保持已有 panic hook 的恢复链；先释放终端交互状态，再进行有界、可报告的异步 Runtime 清理。两种责任分别聚合错误。不要把“drop guard”说成能够恢复 SIGKILL 后的终端，也不要改坏 Core 的取消/效果结算。

**反例**：开启 raw 后注入进入 alternate、创建/绘制终端失败；session 返回 Err；Runtime shutdown 慢；panic。真实终端状态/PTY 验证属于独立测试，不可用 CaptureSink 代替。

来源：[crates/agent-tui/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/main.rs)；[crates/agent-runtime/src/instance.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-runtime/src/instance.rs)；[Ratatui panic hook 指引](https://ratatui.rs/recipes/apps/panic-hooks/)。

## U6：操作命令要保持用户顺序，慢命令不能占住输入循环

`/focus`、`/task`、`/done`、`/continue` 等各自启动 detached tokio task。Actor 只串行化“已经到达”的命令，不保证这些任务按用户键入顺序开始发送。`/task B` 紧接 `/continue` 的第二项可能先到，而 continue 使用无 expected_task_id 的兼容方法。Runtime 现有 Continue/Suspend/Cancel 已提供身份期望接口；TUI 应优先使用它们。

另一方面，`/checkpoint`、`/restore`（含磁盘读取）在 dispatch_line 中直接 await。即使保存是 idle-only，慢盘/较大恢复也会占住唯一的输入/绘制循环，使 Ctrl-C 或取消键无法及时处理。不要据此夸大为“checkpoint 必然与审批死锁”：现有测试明确表明忙时保存可以拒绝。

**最小修复**：一个有界、保序的命令提交队列和可观测回执；普通任务动作按输入顺序提交，控制取消/退出有独立响应机会；磁盘读写离开绘制循环。每帧事件 drain 也应有条数/时间预算。对当前任务动作绑定可信身份，CompleteTask 若需要相同保证就在共享接口增加精确期望，不在 TUI 自行完成任务。

**反例**：延迟 /task 的发送，随后 /continue，不得推进旧任务；恢复读取被暂停时仍能处理退出/取消；回执过期不覆盖新焦点；事件洪泛时键盘响应仍有界。

来源：[crates/agent-tui/src/session.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/session.rs)；[crates/agent-runtime/src/command.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-runtime/src/command.rs)；[crates/agent-runtime/src/instance.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-runtime/src/instance.rs)。

## U7：headless 的成功必须建立在足够完整的证据上

run_headless 收到 broadcast Lagged 只写 warning 然后继续，没有将事件覆盖缺口加入 Drain 和 session_end。丢失段可能包含 ApprovalDenied、Failure 或结算事实；稍后若收到 TurnCompleted，当前已见记录仍可能导出 exit 0。

另一个防御缺口是 Closed 直接 break，而 Drain.finish 最后在没有确认 turn_completed 时也能返回 completed/0。**但正常 RuntimeHandle 本身持有 broadcast Sender**，不能把 Closed 路径直接描述为“正常 actor 死亡必然触发的生产故障”；正常组合可能一直等到 timeout。此项应当作为接口边界用例，与可达的 Lagged 完整性问题分开。

输出队列有界是正确基础，但 timeout/输出失败后，run_headless 先等待 sink.finish，外层之后才开始 shutdown；取消应该在需要结束工作的判定产生时启动，而不是等最多十秒的输出收尾后才发起。返回的 writer 又被同步 flush 一次，不再受该 writer 线程的 close bound 约束。

**最小修复**：成功要求相关任务/回合的正面终态证据；丢失事件先从可信水位补齐，不能补齐就返回明确 incomplete/failure，而不是伪造失败原因或补零。取消与输出排空分别结算；sink failure/timeout 立即进入 Core 取消/停止流程，保留未知效果，不盲重放；所有最后的 I/O 仍受声明边界约束。

**反例**：被丢区间含拒绝事件而尾部有 TurnCompleted，不能按“完整成功”处理；错误接入的已关闭接收器无终态不得成功；慢 writer 下取消不等待输出 flush；执行状态和最终 JSONL/进程 exit 一致。

来源：[crates/agent-tui/src/cli.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/cli.rs)；[crates/agent-runtime/src/command.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-runtime/src/command.rs)。

## B1：逐个 required 解析成功，不代表规划时它们仍在热表

W1 增加了 resolve_required_cold_refs，是合理改进。但每个 exact ID 都调用 hydrate_card_for_outcome；该函数安装条目后立即 settle_metadata_residency，protect 只包含刚安装的一个 ID。解析器只保存 Installed/AlreadyOwned 枚举，没有持有已验证 owner 快照。等整批结束，plan_required_with_resolution 才根据热目录查找。

条件反例：热容量 2；三个可降级的冷条目 A/B/C 都是本轮 required，正文总量能够放入模型预算。依次解析 A、B、C 时，C 的安装将 A 放回 pending。规划 A 不再找到 loaded owner；Installed 在 miss_reason 中又映射为 Missing。数据并未不存在。

**最小修复**：解析时直接生成有界的、版本/范围绑定的 RequiredPlanSource（或一个明确受预算约束的短期租赁）；不能要求所有 required 同时永久驻留热表。短期计划内存也要计入预算。预算不足时使用准确的 BudgetExcluded/UnreadColdPage，而不是宣称 Missing。不通过全历史加载或无上限 pin 解决。

**反例**：required 数超过热 cap、单条很大、混合 exact ID 与路径、前景取回挤占已解析 required。完成解析→材料化→最终 packing 后要么完整呈现，要么给真实预算原因。

来源：[crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-simple/src/engine.rs) 中 resolve_required_cold_refs / hydrate_card_for_outcome / materialize；[crates/context-simple/src/materializer.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-simple/src/materializer.rs) 中 RequiredColdResolution / plan_required_with_resolution。

## B2：内容寻址名称不是内容已验证的证明

run_external_spill_io 的 plan.writes 分支遇到已有路径，执行 try_exists 后直接把条目放入 io.written/io.spilled；既未比较现有内容与计划 bytes，也未走受检卡片读取。checkpoint 随后 record_card，并从 inline external 中排除这些 spilled IDs。

故障反例：预期卡片名已经存在，但内容截断/损坏；当前热内存仍有完整元数据。capture 成功并把热元数据从 checkpoint inline 段移除。下一次 restore 才发现卡片坏了，发生本可在捕获时避免的降级。

**最小修复**：在首次认领一个未验证 existing card 时做有界校验（identity/schema/hash 与计划内容一致），或按内容寻址 store 的安全规则原子写入正确内容。无法验证/修复时本次保留 inline，不抛弃唯一可靠副本。不要每轮无条件重读全部已验证不可变卡片；重点是不能仅凭 exists 把未验证路径升级成新 claim。

**反例**：在无有效 card claim 的 fixture 中预置同名坏文件→checkpoint→新引擎 restore，原元数据必须仍可恢复，或 capture 明确拒绝/保留 inline；覆盖 existing directory、不匹配 hash、读取权限故障。此处不声称存在远程攻击入口，也不声称原文 blob 立即被删除。

来源：[crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-simple/src/engine.rs) 中 run_external_spill_io / checkpoint。

## B3：ledger 导出取消时，错误分支不会替被丢弃的 future 回滚

export_ledger 在写入前 mem::take(state.ledger)，然后 await 写临时文件和 rename。返回 Err 时 merge_back 是有的；但整个 future 在 await 中被丢弃时，不会继续运行这个 Err 分支，记录随局部变量消失。

**最小修复**：锁内复制有界 export snapshot，成功提交文件后才从缓冲中确认相应记录；或使用能够保证取消恢复所有权的等价机制。保留已有串行 gate，不引入新日志系统。

**反例**：在 write/rename 边界暂停并取消导出，记录仍在内存或在已确认的完整产物中；下一次导出不无故丢失或重复；正常 I/O Err 与取消分别测试。

范围仅是 Context 生命周期 ledger 的内存导出缓冲，**不是 Core authority WAL，也不能说成任务所有权丢失**。列为后续收口，优先级低于审批、状态和 required 证据。

来源：[crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-simple/src/engine.rs) 中 export_ledger。

## 较小问题与配置风险（不设独立大门禁）

**Doctor 分页误计数**：run_doctor_checks 只取 CheckpointStore.list(5)，却拿全目录 .json 数减这五项，把第六个及以后的合法 checkpoint 也视作未识别。改用覆盖完整的计数或直接标记“只检查最近五项”。

**开发自动审批覆盖 headless**：main 的 AGENT_AUTO_APPROVE 分支在 is_headless 之前；设置该环境变量时，headless 的“不授权则拒绝”辅助函数没有被调用。它是明确配置的 permissive 模式，不是模型绕过审批，但与 CLI 的默认安全承诺需要对齐：产品入口禁用或明确要求显式启用，并显示真实有效策略。不要把开发开关隐式带入自动化生产会话。

**JSONL 双换行**：caller 和 write_line 都追加换行。大多数忽略空行的消费者不会坏，但不应让测试的宽松解析掩盖正式格式偏差。

**测试与产品组合分歧**：cli.rs 的 test helper 已使用 ComposeConfig::product_baseline，main 仍有另一份大字面量配置。通过共享、可核对的有效配置收敛；保留 TUI 和 host 的明确差异（例如不同 Context 默认值），不要借重构偷偷更改实验/产品策略。

来源：[crates/agent-tui/src/doctor.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/doctor.rs)；[crates/agent-tui/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/main.rs)；[crates/agent-tui/src/cli.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/cli.rs)；[crates/agent-tui/src/args.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/args.rs)。

## 维护性与下一阶段

本轮不建议先拆大文件。优先减少“同一规则被写在几处”的次数：一个共享状态 reducer；一个保序命令入口；一个真实布局/宽度模型；一个 required 解析计划；一个存储提交与所有权结算边界。

TUI 的 session tests 使用 CaptureSink 拼接全部 AppState 消息，不经过 ui::render 或终端裁剪；这能证明会话动作，不足以代替屏幕与终端清理验收。保留这些测试，再补 TestBackend 的关键布局断言和少量真实进程/PTY 异常收尾测试。不要抛弃已有回归后重建一个庞大评测系统。

三条后端线保持：A 执行核心/工具/操作入口（含 TUI），B Context/GC/搜索，C 平台/KV/成本。先处理审批与恢复完整性，以及状态/命令身份；required 批量解析并行。U2 渲染是支持真实代码阅读的必要功能，不是 GUI 装饰。Doctor、ledger 导出等小项不阻塞主体推进。

供应商 KV 不在本轮重新开“断点类型接线”旧任务。下一步沿实际 request 序列验证稳定策略/阶段证据与动态尾部的变化、缓存读写和每次尝试账目。TUI 显示必须保留 unknown/partial usage，重放不得重复累计。最终按同质量交付的任务总成本评价，不按缓存命中率单独宣告收益；真实付费实验继续作为有凭据、有授权预算时的条件任务。

详细派工及停止条件见 [NEXT_ACTIONS.md](NEXT_ACTIONS.md)。本报告不要全文追加到 CURRENT/NEXT_TASKS；当前视图只维护仍需执行的下一动作，历史证据用链接保留。
