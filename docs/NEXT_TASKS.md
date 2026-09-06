# 可执行任务队列

> 状态：**当前工单是 PROCESS-01 Unix 剩余**（审查其余代码项已关闭；M16-02 已于 2026-09-07 关闭；PACKAGE-01、MCP-01 按条件）。
> 审查剩余排在 M16 产品剩余之前；不是第二套队列，也不是把 13 项清零当成新阶段。
> 审查/提案基线：`12c86283b8d5991e9f17a07f14871dcf39d65066`。本工作树 HEAD：`4464640`。
> 分流原文：[reviews/2026-09-06-deep-audit/REVIEW.md](reviews/2026-09-06-deep-audit/REVIEW.md)。建议回归：[reviews/2026-09-06-deep-audit/TEST_MATRIX.md](reviews/2026-09-06-deep-audit/TEST_MATRIX.md)。
> 不替代 Core、Effect、Workspace、恢复与输出边界契约；不改写历史评测结论。

## 开始执行

只读 [CURRENT.md](CURRENT.md) 和本表当前工单，然后读该工单的实现、调用者和测试。
已完成的项定向确认后跳过。一次只做一个工单。
代码存在、测试通过、默认启用、真实任务跑通是四个不同事实。

不要把 Chronicle、TaskGraph、数据库、worker、向量或新全量评测框架当作前置。

## 当前队列

| 顺序 | 工单 | 交付物 | 状态 |
|---|---|---|---|
| 1 | ~~STORAGE-02~~ | 压缩已发布后失败则隔离旧 writer | 已关闭（2026-09-06） |
| 2 | PROCESS-01 | 宿主验证硬崩溃监督 | 部分关闭：Windows 围栏已落地；Unix pre_exec 在 GH runner 上静默不执行（探针实证），需持久监督身份设计 |
| 3 | ~~PROCESS-02~~ | reap 未确认退出不清 pid | 已关闭（2026-09-06） |
| 4 | ~~WORKSPACE-01~~ | 普通 open 不阻塞 FIFO | 已关闭（2026-09-06） |
| 5 | ~~WORKSPACE-02~~ | Windows 拒绝路径立即接管 HANDLE | 已关闭（2026-09-06） |
| 6 | ~~PROVIDER-01~~ | 错误 HTTP body 有界读取 | 已关闭（2026-09-06） |
| 7 | ~~PROVIDER-02~~ | Chat `length` 终止语义 | 已关闭（2026-09-06） |
| 8 | ~~PROVIDER-03~~ | Responses EOF 尾帧校验 | 已关闭（2026-09-06） |
| 9 | ~~CONTEXT-01~~ | 依赖候选 newest-first | 已关闭（2026-09-06） |
| 10 | PACKAGE-01 | 打包来源绑定 | 下次实际发布 |
| 11 | MCP-01 | MCP 写/连接/读可取消 | 仅默认启用 MCP 时 |
| 12 | ~~M16-02 剩余~~ | 待审阅 ≠ 持久完成 | 已关闭（2026-09-07） |

已关闭、跳过：STORAGE-01、DOC-01；EOF wait、消费 ACK、PromptRequired、resync。
关闭证据：STORAGE-02 `f9852ea`、PROCESS-01/02 `7c72df3`、WORKSPACE-01/02 `17c5ded`、PROVIDER-01/02/03 与 CONTEXT-01 `3e0128a`（定向测试计数见各提交说明）。
M16-00/01/02/05/07 与大部分 03/06 已落地，细节见下方 M16 表。

---

## STORAGE-02：压缩发布后的 writer 围栏（已关闭 2026-09-06）

**实现：** `compact_locked` 的全部可失败步骤（新 WAL seek、next_seq 溢出检查、恢复态重建）移到 `persist_authority_metadata` 发布点之前；发布后只剩纯内存 writer 交换与尽力删除旧 WAL，结构上不可能再把 writer 留在旧代。新增定向测试：新 WAL 创建失败（发布前）保持旧代可追加且元数据未动；发布后/删除旧 WAL 前的崩溃窗口重开按已发布代际续写。agent-storage 21/21。

**用户结果：** 压缩元数据已经写到磁盘之后，若 seek、切 writer 或显式 compact 调用失败，本进程不能继续往旧代追加；再次打开与对账结果一致。

**入口：** `crates/agent-storage/src/lib.rs`（`compact_locked`、`persist_authority_metadata`）；`crates/agent-core/src/operation.rs`（`compact_authority_journal` 目前只转发错误）。普通 `append_transition` 已有围栏，不要拆掉。

**步骤：** 读 compact 成功发布 metadata 之后的失败返回点；失败后隔离旧 writer（或等价的本进程写入围栏）；打开路径与 STORAGE-01 一致：缺 metadata 且有代际则 `RecoveryRequired`，不选最大 `.gN`。

**检查：** `cargo test -p agent-storage`；若改了 Core 转发路径，加一条显式 compact 失败后不可再追加的定向测试。不跑全仓。不注入真实 Windows 进程崩溃。

**做到这里就停。** 不建 Chronicle；不把 STORAGE-01 的 Windows 崩溃注入当成本工单。

---

## PROCESS-01：宿主验证硬崩溃后的监督

**用户结果：** 开启宿主验证时，宿主进程被 SIGKILL/abort 后，不留下可继续改工作区的无监督子进程。

**入口：** `crates/tool-runtime/src/proof_runner.rs`、`crates/tool-runtime/src/tools/process.rs`。Linux 探针已证 OS 机制（父死子存），不是 Agent 集成测试。

**检查：** 定向 `tool-runtime` / 现有 crash fixture；监督身份与权限身份分开。不否定已落地的取消桥接（F3-6b）。

**不要：** 为这件事新建 Scheduler。未跑 Agent crash-resume 不写已闭环。

---

## PROCESS-02：未确认退出不清 pid（已关闭 2026-09-06）

**实现：** `reap` 返回类型化 `ProcessReapOutcome`，仅在确认退出后清 pid，未确认终态由 Drop 保留击杀责任；`kill_tree` 直接子进程 fallback 改为无锁 direct pid kill（原 `try_lock` 在 reap 持锁时必失败）。回归：持锁期间非 group-leader 子进程经 fallback 终止（`7c72df3`；agent-process 30）。

**用户结果：** `reap` 在 wait 失败或两次有界等待都超时时，不能报告清理成功并丢掉监督身份。

**入口：** `crates/agent-process/src/supervisor.rs`（`ProcessSupervisor::reap`、`kill_tree`）。

**检查：** 注入 wait 错误 / 第二次 wait 超时的定向测试。Unix group leader 不能当作本工单已证明。

**不要：** 用“再 kill 一次”冒充确认退出。

---

## WORKSPACE-01：普通 confined open 不阻塞 FIFO（已关闭 2026-09-06）

**实现：** 普通 open 带 `O_NONBLOCK`（普通文件/目录 I/O 不受影响），同句柄 stat 拒绝非普通文件/目录，staged 目标要求普通文件；`project_markers` 改为仅元数据探测（`fstatat AT_SYMLINK_NOFOLLOW`），根扫描不再打开任何条目。回归：无写端 FIFO 位于 `Cargo.toml` 不再卡住扫描（watchdog 测试；`17c5ded`，agent-workspace 98+5+3）。

**用户结果：** 项目标记探测遇到无写端 FIFO 时有界失败，不在同步 `open` 上卡死。

**入口：** `crates/agent-workspace/src/confined.rs`、`runtime_facts.rs`（`project_markers`）。recovery 路径已有 `O_NONBLOCK`，复用它。

**检查：** Unix FIFO 名为 `Cargo.toml` 的定向测试；普通文件、`.git` 目录、symlink/reparse 不被误伤。正文只进入支持的文件类型。

**不要：** 把所有标记都当普通文件打开。

---

## WORKSPACE-02：Windows 拒绝路径立刻接管 HANDLE（已关闭 2026-09-06）

**实现：** 六处 raw HANDLE 调用点（`open_root_handle`、`open_child_dir`、`open_existing` 两臂、`open_staged_for_cleanup`、`open_or_create_regular_file`）全部先 `from_raw_handle` 接管再 `check_not_reparse`，对齐 recovery helper 模式。既有 reparse 拒绝测试覆盖行为；句柄计数故障注入未做（`17c5ded`）。

**用户结果：** `check_not_reparse` 失败时句柄仍被拥有并关闭，拒绝仍然发生。

**入口：** `crates/agent-workspace/src/confined.rs` 多处 raw HANDLE。复用已有 recovery helper。

**检查：** 现有 Windows confined 拒绝测试仍通过。未做句柄计数故障注入则写明。

**不要：** 把“拒绝发生”说成路径逃逸已存在。

---

## PROVIDER-01：错误响应有界读取

**用户结果：** 非 2xx 的巨大或持续 body 在读入阶段被限额，不先 `text().await` 再截短。

**入口：** `crates/provider-openai/src/lib.rs`（`complete_chat_stream` / `complete_responses_stream`）。

**检查：** loopback 夹具：大 body、持续 body、多字节 UTF-8；取消与超时仍正确。

**不要：** 改模型协议权威；无压力测试不写已抗资源耗尽。

---

## PROVIDER-02：Chat length 终止

**用户结果：** Chat `finish_reason=length` 与 Responses 输出上限一样，保留不完整终止，不丢成正常完成。已暴露的输出不透明重放。

**入口：** `crates/provider-openai/src/sse.rs`、`lib.rs`、`responses.rs`。

**检查：** Chat length+DONE 与 Responses `max_output_tokens` incomplete 成对夹具。

**不要：** 无条件重试。

---

## PROVIDER-03：Responses EOF 尾帧校验

**用户结果：** 最后一帧无论是否空行结束，event 名与 JSON type 矛盾时都拒绝。

**入口：** `complete_responses_stream` 的 `framer.finish()` 路径；与正常 SSE 共用 handler。

**检查：** 同一矛盾帧有/无尾部空行的夹具。

---

## CONTEXT-01：依赖候选 newest-first

**用户结果：** 同一实体超过 64 条 live 条目时，候选仍按 newest-first，而不是先截创建序旧前缀再排序。

**入口：** `crates/context-simple/src/index/dependency.rs`（`push_linked`）、`indexes.rs`（`update_entities` / `swap_remove`）。

**检查：** 64/65/128 条；dead 前缀不耗尽配额。扫描工作量与候选配额分开。

**不要：** 重写选择器或调 GC 阈值。关联边不是强制正文 Continuation。

---

## PACKAGE-01：打包来源绑定

**用户结果：** 一次发布复制的二进制就是这次构建写出的那份；旧 `dist/<version>` 和自定义 target 不能拼出“成功”的旧包。

**入口：** `scripts/dist.sh`、`scripts/dist.ps1`、`.github/workflows/package.yml`。Bash 桩测已证明旧产物可被打包。

**何时做：** 下次实际发布或主动打包装时，不要提前改脚本充数。PowerShell 原生退出码一并核对。

**不要：** 宣称当前已发布 ZIP 已错；不为它新建评测框架。

---

## MCP-01：MCP 取消覆盖写阶段

**用户结果：** 对端停读时取消能打断写请求，不等完整 `request_timeout`；半帧 session 被毒化；清理状态有 reap 依据。

**入口：** `crates/agent-capability-process/src/mcp.rs`（`request_with_cancel`）。

**何时做：** 仅当默认产品声明启用 MCP 写路径。当前未启用则跳过，不算阻塞队列里的代码项。

**不要：** 第二调度器。

---

# M16：可持续交付的本地单 Agent

审查剩余关闭后再推进这里的产品剩余。阶段目标不变：用户给仓库级任务，Agent 能短计划、查读修改、补充、有限执行、停止、冷恢复、可审阅结果，以及同一 Runtime 的非交互入口。

提案原文不进默认必读：[reviews/2026-09-06-m16-proposal/TRIAGE.md](reviews/2026-09-06-m16-proposal/TRIAGE.md)。

| 工单 | 交付物 | 本分支状态 | 旧映射 |
|---|---|---|---|
| M16-00 | 切换活动路线 | 已切换 | D0 |
| M16-01 | 继续、忙时补充、启动预检 | 代码已落地；TUI 走查待做 | F1 |
| M16-02 | `/work` `/plan` 与完成语义 | 已落地（2026-09-07：待审阅/持久完成显式区分） | F2 |
| M16-03 | 有限模型轮与可确认取消 | 主体已落地；PROCESS/PROVIDER 见上列 | F3 |
| M16-04 | 可信冷恢复 | STORAGE-01/02 已落地；产品配置恢复走查待做 | 恢复路径 |
| M16-05 | `/review` 与状态区分 | 结果卡已落地；双工作区走查待做 | F4 |
| M16-06 | 上下文/搜索正确性 | 主体已落地；CONTEXT/WORKSPACE 见上列 | F5 |
| M16-07 | 单进程非交互入口 | N1/N2 已落地 | 原 F6 之后项 |
| M16-08 | 试用包与三类走查收口 | 无头 live 已有；PACKAGE-01 见上列 | F6 + 发布 |

---

## M16-00：切换活动路线

**用户结果：** 开发者进入仓库后看到一条当前队列，不会回到 M15 候选选择或 Chronicle 建设令。

**已做（2026-09-06）：** CURRENT / ROADMAP / 本文件改为 M16；随后把深入续审仍开放项提到本队列前面。提案原文入库审查目录。文档检查：`python scripts/doc_consistency.py`。

---

## M16-01：接通交互控制与可信启动

**用户结果：** 能继续原任务；忙时补充能看到受理/排队/拒绝；纯配置错误不先建 workspace 状态。

**已落地：** `/continue` → `continue_active_task()`；忙时单槽队列；命令错误走 notice；未知 flag 拒绝；release 消费盖章与 PromptRequired 判重（`c6fbbab`）。参数解析在打开 workspace 之前。

**仍待做：** TUI 手工走查（恢复后 continue、忙时第二条输入）。模型 key 校验仍在 workspace 打开之后——若再动启动顺序，只前移纯配置错误，不抽 CompositionPlan。

**不要：** 用 `user_message("继续")` 代替 continue；无界输入队列；后台服务。

---

## M16-02：任务工作模式、短计划与完成语义（已关闭 2026-09-07）

**已全部落地。** 入口（`/work`、`/plan`）与完成语义展示均已关闭；细节见下。

**用户结果：** 一个开发目标有短清单；用户能区分「已产出待审阅」「本段结束」「证据完成」「操作员接受」。

**已落地：** `/work`（focus + 空需求集上 `task.manage` PreferSurface + 一次 user-message）；`/plan` 读只读 `TaskPlanView`；清单随检查点往返。

**本切片剩余：已全部落地（2026-09-07）。**

1. 默认 `OperatorClosureOnly` 在 TUI / 无头结果里明确显示为待操作员审阅关闭。普通 final 结束 turn，不显示为持久 `TaskCompleted`。**已落地：StatusProjection 任务行带 [awaiting operator review] / [durably completed (operator accepted)]；TUI turn 结束时对活动任务显式提示；无头 session_end 增 `task_state` 字段（operator_accepted / awaiting_operator_review / none）。**
2. `/done` 走既有操作员接受路径，不伪造验证 PASS。**既有路径未动。**
3. `EvidenceRequired` 仅在宿主已声明准则与 coverage 时使用；普通 `cargo test` / `npm test` 保持 `TaskScoped`。**既有语义未动。**
4. `next_action` 仍是建议；不增加“必须为空才能完成”。计划 `[x]` 不是证据。**未变。**

没有可信验收域时，把结果交给用户审阅就是完整产品行为，不要为此新建通用验证器。

**检查：** 定向 `agent-tui` 状态投影 / 无头 `session_end`；复用现有 OperatorClosureOnly / task.manage 回归。不跑全仓。

---

## M16-03：有限执行段与可确认的进程取消

**用户结果：** 较长任务在有限模型轮后安全让出并显式续跑；慢命令/验证不会让控制入口失去响应。

**已落地：** `--max-rounds` 按模型轮解析；RoundBudget 让出并落安全点；EOF 后仍守超时/取消；验证取消 token 桥接 Actor。

**代码缺口改走前列 PROCESS/PROVIDER。** `--defer-proof` 改产品默认仍等真实慢验证走查。

**不要：** UI 自动无限续跑绕过用户上限；独立 Scheduler / worker pool。

---

## M16-04：可信冷恢复与最小历史入口

**用户结果：** 关掉进程后能选经过验证的检查点，恢复同一任务并继续；缺失 metadata 不能当成新工程。

**已落地：** STORAGE-01；`RuntimeInstance::restore`；`--restore=latest`；`route_flow.rs` 含跨检查点 continue。

**存储围栏已落地（STORAGE-01/02）。** 之后在产品配置（capability-aware + broker + 非 permissive 审批）下补一条「保存 → 结束进程 → 恢复 → continue」，复用 `crash_resume.rs`，不建第三套评测。

**不要：** Chronicle / RunCatalog 数据库。

---

## M16-05：结果审阅与统一状态展示

**用户结果：** 能看到改了什么、哪些检查真正执行、哪些未验证；`/review` 不调模型、不绕过 Core 跑工具。

**已落地：** 事件派生结果卡；不归属用户原有修改；resync 最新优先/当前 run/水位/有界读；run_summary 按 entries+omitted。

**仍待做：** 双工作区走查（干净 + 含用户旧修改）；展示上继续分清待审阅 / 预算让出 / 持久完成 / 恢复受阻，不能只凭 `TurnCompleted` 或 `RunCompleted` 推断业务完成。

**不要：** 全仓 `git diff HEAD` 据为 Agent 成果；diff 编辑器；自动 rollback。

---

## M16-06：上下文、GC 与搜索的实际编码闭环

**用户结果：** 跨文件切换能找回正确片段；搜索不完整不冒充全仓无命中。

**已落地：** fs.read 区间事实；同修订窗口覆盖才取代；grep/list PARTIAL；confined 有界读；错误仅 verify.run 可清。

**CONTEXT-01 / WORKSPACE-01 / WORKSPACE-02 走前列。** 最终曝光 ACK 与片段身份的剩余一致性按现有契约小修，不重写 Frame 编译器。

**不要：** 向量库、BM25 新服务、SIEVE/TinyLFU、learned router 作为本阶段必需。

---

## M16-07：可脚本调用的单进程运行入口

**用户结果：** 不用 TUI 也能跑有边界的任务，得到 JSONL 与诚实退出码。

**已落地：** 同一 `agent-tui`：`--prompt` / `--work` / `--continue` / `--grant-file` / `--jsonl-out`。无人审批时拒绝写入；无 `--yes`。退出码 0/2/3/1。

**仍待做：** 若 M16-02 区分了待审阅，无头 `session_end` 与之对齐；不要为脚本退出 0 把普通 final 写成 verified completion。不新增 daemon。

---

## M16-08：发布可日常试用的版本并结束本阶段

**用户结果：** 对应本次源码的 Linux/Windows 包；三类真实任务记录可审阅。

**已有：** 无头 live 三类工作区记录；空 recipe 表启动失败已修。

**PACKAGE-01 走前列（下次实际发布）。** TUI 交互走查；至少一条带用户原有修改。真实 provider 不可用则 live 写 `NOT_RUN`。

**不要：** 为凑 PASS 放宽标准或扩建评测框架。M16 结束不等于 v0.2 已发布，除非另有明确发布动作。

---

## 防止再卡在测试/文档循环

- 每个工单先给出一个用户动作；测试或文档单独增加不算功能完成。
- 开发跑相关回归；集成沿用现有 CI。
- 回执写清：实现了什么、是否默认产品路径、实际检查、是否真实任务、限制、下一工单。
- 安全与持久性不能靠“快点落地”绕过；真问题修当前路径。
