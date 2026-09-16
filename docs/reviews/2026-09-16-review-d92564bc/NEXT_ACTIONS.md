# 下一步可执行切片：后端与 TUI

基线 `d92564bcfa41dda44f752e1e49e88a321abdf942`。先比较执行工作树与基线；保护用户已有修改。下面测试名称均为建议新增用例，不代表仓库已存在或本轮已运行。发现解释见 [REVIEW.md](REVIEW.md)。

## A1 — 审批可核对与基本文本渲染（U1/U2）

**目标**：用户能在当前审批请求上查看完整有界参数，并能按原行结构读代码/错误/计划。

入口：agent-tui/ui.rs、state.rs 的 PendingApproval/UiMessage、session.rs 的审批输入处理。

先补红用例：真实 TestBackend 渲染长参数尾部；多行代码的第二行与缩进；窄窗翻页；中文输入光标；超八参数有继续查看路径。确认与 request_id 一起测试，拒绝旧请求被新请求替换后继续使用旧确认。

实现：完整审批数据/有界详情引用＋可滚动面板；Text/Line 正确转换；共用布局宽度、滚动计数与光标列。

停止：这些实际渲染/输入用例通过且批准/拒绝仍走同一 Core gate。不要扩展主题、插件面板或 GUI。

## A2 — 一份事件读模型与任务 review（U3/U4）

入口：agent-runtime/status.rs，agent-tui/state.rs/session.rs。

先补：实时与 replay+redelivery 等价；重复 ModelUsed/TurnCompleted 不双计；取消无 usage 清理 in-flight；相同文本不同 turn 保留；A 完成→B 失败 review 不复用 A 完成头；第 33 个 check 的失败不被静默藏掉；坏日志区间保持 partial。

实现：共享 reducer、严格事件身份/序列、可靠 snapshot 水位＋tail；task-bound review；遗漏总数与查看窗口分离；原子版本化卡片写入。不要从 prose 推断完成或验证。

停止：现场画面、/status、/review 与相同已验证事件视图一致，旧事件不能反向激活操作。保留草稿/滚动为 ViewState，不建新任务权威。

## A3 — 保序控制与可退出的终端（U5/U6）

入口：agent-tui/main.rs/session.rs，必要时 command.rs 的精确完成期望。

先补：开启 raw 后任一初始化失败仍恢复；慢恢复期间可响应取消/退出；人为延迟 /task 的发送后 /continue 不作用于旧任务；已取消回执不覆盖新代际。

实现：TerminalSession guard＋panic hook；有界保序命令提交与回执；慢磁盘操作离开事件循环；每帧 drain 有预算；使用已有 expecting/reporting RuntimeHandle 方法。

停止：不会因前端调度重排用户动作；终端先恢复，Runtime 按既有规则完成取消与清理。不把 TUI 强制改成 IPC 客户端。

## A4 — Headless 正面终态与覆盖（U7）

入口：agent-tui/cli.rs/main.rs，既有 runtime snapshot/event journal。

先补：Lagged 丢掉失败/拒绝、尾部 TurnCompleted，结果不得宣称完整成功；无终态的独立 Closed receiver 不得成功（明确这是防御接口用例，不是正常 handle 必然可达）；慢 writer 不阻塞取消；最后 flush 受边界控制。

实现：补齐事件或显式 incomplete；成功需要相关终态；停止工作与输出收尾分离；修双 LF；保持现有出口含义或对新 outcome 做明确版本兼容。

停止：事件流、session_end、exit 与实际结算/完整性一致。未知不冒充拒绝、失败或零费用。

## B1 — 多 required 的有界解析计划

入口：context-simple/engine.rs resolve_required_cold_refs、materializer.rs RequiredColdResolution/RequiredPlanSource。

红例：hot cap=2，required A/B/C 三个合法可降级冷页，模型 budget 足够。读取均成功，A 不得因随后驱逐又成为 Missing。再覆盖 entity/foreground 干扰与真实预算不足。

实现：在验证成功时捕获有限的 owner/版本/范围绑定计划，避免依赖最后的热驻留集合。短期计划计入预算；不得无限 pin 或全量 hydration。

停止：材料化与最终装箱要么给出正确正文，要么报告真正的容量/读取原因。

## B2 — 捕获存储证明与取消安全（B2/B3）

入口：context-simple/engine.rs run_external_spill_io/checkpoint/export_ledger，复用现有受检 store 读写。

红例：无有效 claim 时预置同名坏 card，capture 后新引擎 restore 不能丢失唯一可靠元数据；导出在 write/rename await 中取消，ledger 行仍有归属。

实现：未验证 existing card 不得仅凭 exists 认领；不确定时保留 inline。导出 snapshot 在提交确认后再消费。

停止：不引入新存储层；已有有效不可变 claim 的复用性能不被无差别全量重读破坏。ledger 子项可晚于恢复卡片完整性。

## C — 实际请求的 KV 与成本比较（不重开已修断点类型）

沿现有 provider diagnostics/operation usage 记录，核对连续请求首差异、稳定证据范围与工具 schema 变化，核对失败/取消的已知用量只结算一次。TUI 不重算一份错误账目。

本地先验证最终 wire 与用量路径；真实端点接受、命中和总成本按授权预算条件分别执行/记录 NOT_RUN。质量、指令、证据新鲜度、权限撤销不得因缓存而倒退。GUI 后置。

## 检查命令（供执行者运行；本轮未执行）

```bash
# 每片先运行该模块新增的定向回归，再运行相关 crate。
cargo fmt --all -- --check
cargo test -p agent-tui
cargo test -p context-simple
# 修改 shared status/command 或正式结算时执行相关集成。
cargo test -p agent-runtime -p agent-compose
# 修改 adapter/wire 才跑相应 parity。
cargo test -p context-contextcore -p agent-context-service
# 合并时做相关 clippy；阶段收尾才要求全 workspace。
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

不要每个小改动都重跑付费长任务；不以全部 P2 永久归零作为功能继续推进的条件。保留测试输出对应的精确 SHA、平台、profile 和是否实际调用供应商。
