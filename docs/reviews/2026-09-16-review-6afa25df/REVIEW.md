# 后端、供应商缓存与客户端续审

固定基线：`6afa25dff0230fec982ee7d836aa7811ad901d0c`  
提交时间：2026-09-16 18:19:21 UTC / 2026-09-17 03:19:21 Asia/Tokyo。  
上一基线：`3bdb269cba4e1bdd0fe2e71007dfa1ad7640b28d`。

## 结论与证据边界

最需要先修的是：B1 已让一份仍在冷页中的必需正文进入模型请求，但消费 ACK 仍只认可四种已加载 owner。因此合法结果可能在 Provider 返回后被拒绝，已知用量也在正式发布之前丢失。SDK 则存在“连接可用、事件流已经永久结束”的中间状态；TUI 的 worker 生命周期没有覆盖异常退出。

这是全仓范围的继续审查，实际深读范围见 COVERAGE.md；**没有完成所有源码、测试、SDK、脚本和文档的逐行覆盖**。目录树、变更文件清单、旧报告和本轮源代码阅读是不同证据层级。全部结论固定到上述 SHA；搜索索引返回其他 ref 的片段没有替代固定版本源码。

本轮没有修改或推送仓库；没有运行新增 Rust/.NET 回归、真实终端或付费模型实验。Git 远程访问遇到 DNS 错误，环境未发现 Cargo、Rustc、dotnet；归档下载也未成功。下述回归均为待实施/待执行设计，不是已跑出的失败日志。

该 SHA 的 CI run `35134020808` 在本轮最后一次读取时为 **in_progress，attempt 1，conclusion=null**。不借用父提交绿色状态，不据此推断本轮发现已被既有 CI 覆盖。

## 问题总表

| ID | 建议优先级 | 类型 | 结论 |
|---|---|---|---|
| Q1 | 优先修 / 条件性长流程阻断 | Context → Runtime | 冷 required 已进入请求，但 ACK 不认可 pending cold owner。 |
| Q2 | P2 | Runtime 成本完整性 | ACK 失败先于 ModelUsed，已经完成的模型调用用量未入账。 |
| Q3 | P2 | Provider 成本完整性 | 流内多种提前退出绕过 accumulator 用量结算。 |
| Q4 | P2 | SDK 可恢复性 | Session 队列溢出后连接仍健康，普通查询不能重建事件流。 |
| Q5 | P2 / 条件竞争 | SDK 重同步顺序 | 快照、连接、队列、pump 的发布不是同一个代际边界。 |
| Q6 | P2 | TUI 生命周期 | 绘制/读键异常通过 `?` 退出，绕过 worker 的 abort/join。 |

优先级是建议修复顺序，不代表每项在默认配置下必现。未观察到实际用户数据删除、跨账户缓存泄漏或权限绕过。

## Q1 — 必需冷正文能够材料化，但消费确认仍认为它没有 owner

### 证据链

- [crates/context-simple/src/engine.rs:2340–2535](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/context-simple/src/engine.rs#L2340-L2535)：B1 解析捕获 owner 快照；`has_exactly_one_owner` 仍仅统计 heap、Warm、`pending_externalize_retry` 和 loaded external。
- [crates/context-simple/src/engine.rs:3220–3555](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/context-simple/src/engine.rs#L3220-L3555)：材料化创建 PendingMaterialization；ACK 先核对预览 ID，然后用上述函数验证 owner。
- [crates/context-simple/src/access.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/context-simple/src/access.rs)：`stamp_consumed` / `stamp` 同样没有 pending cold 的更新路径。
- [crates/agent-runtime/src/actor/model.rs:1370–1600](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/agent-runtime/src/actor/model.rs#L1370-L1600)：最终 ACK 的 `item_ids` 直接来自最终 `materialized.items`，不是只取仍驻留的记录。
- [crates/agent-runtime/src/actor/tools.rs:1010–1220](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/agent-runtime/src/actor/tools.rs#L1010-L1220)：ACK 错误使本轮模型结果停止提交。
- [crates/context-simple/src/tests/batch_required_plan.rs:1–220](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/context-simple/src/tests/batch_required_plan.rs#L1-L220)：现有三目标/热容量二的回归，已经证明 A 回到 pending 且三份正文都进入 items，但测试停在 materialize，没有消费 ACK。

### 最小条件反例

```text
热元数据容量 = 2；required = A、B、C；三份正文能放入最终模型窗口。
B1 解析捕获 A/B/C 后，C 的安装将 A 降回 pending cold。
材料化仍正确返回 A/B/C；Runtime 最终 ACK 也列出 A/B/C。
Provider 返回成功结果。
ACK 校验 A：四个已加载 owner 计数都为零 → 拒绝。
```

这不是正文再次 Missing，而是材料化之后的提交协议拒绝了有效消费。它不等于磁盘内容被删除。

### 最小修复

统一逻辑 owner 查询，让 pending card 的 `(item_id, card/version)` 也是可验证 owner。消费记录必须绑定已发出的 materialization/最终 ID 集与原 owner 版本；不能仅以“pending 里出现过同 ID”为充分证据。

**不能只把 pending 加进 owner 计数。** `access::stamp` 仍无法更新它，debug 可能触发断言，release 也不能把没有发生的强化说成成功。应给本次有界消费保留可靠更新路径：例如按已验证定位逐项写入/结算，或沿现有 checkpoint 状态保存有界、持久化的访问增量。具体实现可复用原目录，但应保证最终热预算与取消安全，不永久 pin 全部 required，也不全量重水化历史。

### 验收

复用 batch_required_plan 的原 fixture，增加完整 ACK，再走真实 Runtime 成功结果提交。断言：三份正文确实进入最终帧；A 确实冷驻留；ACK 成功且不重复强化；热预算不被无限突破；后续 checkpoint/restore 保留必要消费语义。另测错误 materialization ID、外来 ID、过期卡片版本仍被拒绝。

## Q2 — 内部提交失败不应删除已经知道的模型费用

源：[crates/agent-runtime/src/actor/tools.rs:1010–1220](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/agent-runtime/src/actor/tools.rs#L1010-L1220)。

`OperationOutcome::ModelOutput` 当前先提交消费 ACK，失败时发 Error、清空 turn 并返回，`ModelUsed` 发布在它之后。Q1 是一条具体触发路径；独立的 Context ACK I/O/一致性故障也能触发这个顺序。

业务结果不可提交，不能推出这次模型请求没有成本。应将 operation 的使用量结算从业务/Context 接受分支中提取出来；主/维护用途、缓存读写、Observed/Unknown 都保持原始语义，并复用 existing operation 去重规则。ACK 失败仍拒绝工具派发及不可靠结果，不通过放松安全校验来保存账目。

验收：脚本 Provider 返回包含明确 usage 的结果；ContextEngine 在 ACK 注入失败；工具调用不得执行，失败如实报告，正式账目仍收到这次 usage 一次。重复 completion、取消与迟到 completion 不重复收费；日志本身故障时保留可恢复结算义务或明确报告未知，不能声称写入成功。

## Q3 — Provider 流内失败仍能丢掉已解析的 usage

源：[crates/provider-openai/src/lib.rs:470–825](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/provider-openai/src/lib.rs#L470-L825)、[crates/provider-openai/src/lib.rs:800–900](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/provider-openai/src/lib.rs#L800-L900)、[crates/provider-openai/src/sse.rs:280–365](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/provider-openai/src/sse.rs#L280-L365)。

当前已经处理取消与末尾 Done sink 失败的带用量错误，但中间仍有裸提前返回：idle timeout、流/行/帧上限、流 I/O、解析/accumulator 错误及部分 sink 错误。尾部统一读取 usage 的代码覆盖不到这些出口。

一个本地、无需真实供应商的反例：Chat SSE 先送出一个合法的 usage-only chunk（`choices: []` 与明确 input/output/cache counters），随后在 `[DONE]` 前停滞到超时，或发送格式错误数据。`StreamAccumulator::apply` 已保存计数；流函数提前返回却没有把它们封入 typed error。后面的 Retry 和 Runtime 无法恢复未收到的账目。

不把这条反例扩写为“所有 Responses 正常完成都会丢用量”：Responses 完成标记会结束循环；这里讨论的是已知用量存在时的失败出口。

最小实现：一次网络 attempt 的读流逻辑返回结果，外层在所有退出上从同一 accumulator 做一次使用量结算；错误原类型、可重试性、取消分类不变。若已有 error 带同一 attempt 的累计计数，不能重复相加。对多次真实 attempt 才合计其独立费用。

验收矩阵：usage 后 idle timeout / malformed JSON / I/O error / stream cap / sink error，分别验证 reported_usage；无 usage 的同形错误保持 Unknown。再经过实际 RetryingTransport，验证第一次失败成本加最终成功成本恰好各一次。

## Q4 — SDK 自身的事件队列溢出，没有触发真正可用的恢复入口

源：[clients/dotnet/Agent.Client/ResumableSession.cs:1–345](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/clients/dotnet/Agent.Client/ResumableSession.cs#L1-L345)、[clients/dotnet/Agent.Client/ResumableSession.cs:346–600](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/clients/dotnet/Agent.Client/ResumableSession.cs#L346-L600)、[clients/dotnet/Agent.Client/AgentConnection.cs:1–150](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/clients/dotnet/Agent.Client/AgentConnection.cs#L1-L150)。

`PumpEventsAsync` 对 Session 队列溢出会 complete 队列、置 `_eventsOverflowed=true` 后退出，但不会使底层连接失效。`LiveAsync` 只要连接 `IsConnected`，就直接返回它；重建队列的代码只在 `ConnectAsyncCore` 中。因此 Snapshot 还能成功，不代表 Events 会恢复：pump 已经结束，session reader 已关闭。

现有测试 [clients/dotnet/Agent.Client.Tests/EventStreamTests.cs:640–755](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/clients/dotnet/Agent.Client.Tests/EventStreamTests.cs#L640-L755) 在溢出后明确让服务端断开 socket，再等待 IsConnected=false；它证明断网重连路径，不证明底层连接仍健康时的 session-overflow 恢复。

最小修复：把 Session 流健康状态纳入连接/代际可用性判断，或提供显式的、真正重建 snapshot+subscription+pump 的恢复动作。故障对象在锁外清理；新 stream 代际对调用方可见。不要重新发送未知结果的 mutation；snapshot 能确认什么就只声明什么。

验收：容量二，分批送三个 durable 通知，仅使 Session 队列溢出；保持 socket 健康并持续回答查询；执行公开恢复路径后，不依赖服务端断线就能得到新快照、新 reader 和下一条事件。旧 reader 保持终态；审批不得自动重答。

## Q5 — SDK 的重同步发布顺序存在两个可控制的竞争窗口

源：[clients/dotnet/Agent.Client/ResumableSession.cs:160–330](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/clients/dotnet/Agent.Client/ResumableSession.cs#L160-L330)。

窗口一：连接安装之后，在 `Resynced(snapshot)` 之前就启动 `PumpEventsAsync`。若源队列已有快照水位以上的事件，它可以先进入消费者；消费者随后应用旧快照，就可能覆盖刚刚处理的更新。

窗口二：旧 pump 在 `_gate` 内检查“还是当前连接”，释放锁后才向 `_events` 入队。其间新连接可能安装并清队列；旧 pump 再恢复执行，会把旧通知写进新视图。

这两项是源码允许的 interleaving；本轮没有执行 .NET 并发回归，也没有把它说成每次 reconnect 都发生。正常顺序测试不能排除此窗口。

最小修复不是仅把一行 `Resynced` 挪到 pump 前：还需让连接、事件队列与 generation 作为同一发布单元。快照生效有明确屏障，新 pump 之后才能交付更新；旧 pump 的最终入队与代际校验不可跨过安装边界。可复用 `_gate` 与队列，不增加新会话框架。调用外部事件处理器时不要持内部锁。

验收：暂停新 pump/快照回调分别控制顺序；暂停旧 pump 于“检查通过、尚未 enqueue”，安装新代际，再释放旧 pump。验证新视图既不回退，也不混入旧事件。

## Q6 — TUI 正常退出清理了 worker，但异常退出会将它分离

源：[crates/agent-tui/src/session.rs:115–700](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/agent-tui/src/session.rs#L115-L700)。

R6 已把普通 Input 接入有序 worker，应保留。但 `sink.draw(&app)?`、`source.poll_key(...).await?` 与 dispatch 的早退可以绕过循环之后的 `command_worker.abort(); await`。Tokio JoinHandle 被丢弃会分离任务，而不是取消它。底层 Runtime 的安全屏障仍在，但排队动作可能在前端异常收尾期间继续送入 Runtime，且会话不再持有可靠的 join 结果。

最小修复：将会话循环结果与统一 cleanup 分开，所有错误/正常退出汇合。停止接收与不再派发排队项需要由 worker 确认；已送入 Runtime 的命令不因调用方 wait 被丢弃就被称为“未执行”。捕获/恢复本身的取消安全与 Runtime 的提交状态需要沿原接口结算，不把 abort 当成回滚。终端 guard 和命令 worker 是不同资源责任。

验收：worker 正在受控等待、队列有待派发动作时，UiSink 抛错、UiSource 抛错及正常 quit 分别执行；不能在 stop 屏障后偷偷派发剩余命令；in-flight 回执/未知结果诚实，worker 最终被回收，terminal 仍恢复。

## 非阻塞观察（不要各自新建阶段）

### O1 — 资源采样中的 FullTree 不足以证明数值完整

[clients/dotnet/Agent.Client/MetricsSession.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/clients/dotnet/Agent.Client/MetricsSession.cs)：`SafeWorkingSet` 将若干读取错误转换成 0；Parent 将部分读取/解析失败转换成 null；这些情况不一定设置外层 `hadErrors`。只要至少有一个父关系可读，覆盖标签就可能为 FullTree。此外 report 的 coverage 来自最后一次 sample，而 peak/idle 来自其他时刻。

应分开结构覆盖与数值采样完整性，每项聚合保留所用样本的覆盖质量。0 不是“未读到”的通用编码。此项不阻塞本轮主体修复，但不能用这个 FullTree 标签证明完整进程树的精确内存收益。本轮没有执行平台资源实验。

### O2 — 卡片已有字节比对已修，但读取上限应落在实际 handle

[crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/context-simple/src/engine.rs) 的 `run_external_spill_io`（本轮读取了父提交的对应 diff）B2 改动现在比对 planned bytes，避免仅凭存在就认领，应保留。它仍先 pathname metadata 判断等长，再调用整体 read；并发替换/增长会使前置长度检查不再是实际 read 的硬上限。复用 opened-handle + `take(expected_len+1)` 的有界模式即可。此为并发故障边界，不重开原 B2，也未证明远程攻击入口。

### O3 — 命令 pending 计数不是“未执行”的证明

[crates/agent-tui/src/session.rs:115–430](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/agent-tui/src/session.rs#L115-L430)：send 成功后才 `fetch_add`，worker recv 后先 `fetch_sub`，多线程下存在短暂下溢窗口；退出前读取 counter 与 abort 之间也可发生 dequeue。把计数当诊断即可；语义上的“未送入 Runtime”应来自 worker 的停机回执。随 Q6 修正，不单开大任务。

## 供应商 KV 的下一片：可验证请求序列，不是再加一个配置字段

现有稳定路由键的规范摘要（[crates/agent-contracts/src/model_cache.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/agent-contracts/src/model_cache.rs)）与受确认 Responses profile 的内容块放置/explicit-only（[crates/provider-openai/src/lib.rs:960–1220](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6afa25dff0230fec982ee7d836aa7811ad901d0c/crates/provider-openai/src/lib.rs#L960-L1220)）应保留。不重新实现已修的 key 编码或旧 sibling 形状。

当前仍有两种表示：带内容与 tools 摘要的旧 reuse boundary，以及 mapper 直接读取的 `cache_breakpoints` 数字列表。下一片应让缓存计划从最终请求统一验证产出，不让裸索引脱离证据层含义。这是维护性与可观察性收敛；普通合法生产输入并非因此必然错误命中。

在官方所述前缀缓存中，相同 key 不替代相同有效前缀；动态尾部应在稳定边界后。缓存写入与读取也需分别核算，不能仅报告命中比例。本项目无需为实验永久保留失效证据，也不应让 prompt snapshot 留下另一份无界历史。

本地请求序列验证：固定 profile/任务/工具契约，逐轮只改变一个原因——焦点、额外检索、缺失提示、checkpoint 恢复、文件版本、工具撤销；记录最终 wire 的各边界摘要、首次变化原因、输入估计和实际 usage 完整性。必要语义变化必须生效，无意义统计变动不应污染稳定基座。

实际供应商接受、真实 cache read/write 和同质量任务费用分别为条件实验，未运行保持 NOT_RUN。Q2/Q3 未修时，失败/内部提交异常会漏掉已知计数，不能据此宣称降本。

## 开发推进与维护性

保留三条后端线，GUI 不扩展，TUI 作为操作入口：

- B：Q1 逻辑 owner/消费提交闭环，复用 B1 fixture，不再重新实现材料化。
- A/C：Q2/Q3 将一次模型尝试的结算与业务接受分离；改动公共 usage 契约由单一集成人负责。
- C：Q4/Q5 SDK 会话代际与恢复；查询可以恢复，未知 mutation 不自动重发。
- A：Q6 正常/异常退出的统一 worker 生命周期；同步处理 O3。

同一规则减少重复入口，行为修复与机械移动分提交。既有定向测试补上下一跳边界后运行相邻集成，再走原 CI；不加平行评测平台，不以所有观察项永久归零作为主体使用前提。

文档应把“材料化函数通过”与“Runtime 已完成消费提交”分别记录，把“强制 socket 断线后恢复”与“客户端自身溢出即可恢复”分别记录。CURRENT/NEXT 只替换当前行动；报告正文与关闭过程留在本目录链接中。

## 外部语义依据

- Tokio JoinHandle 文档（任务分离、abort 与 join 的区别）：https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html
- OpenAI 官方 Prompt Caching 指南（精确前缀、键、显式边界与读写计数）：https://developers.openai.com/api/docs/guides/prompt-caching

外部资料用于核对通用库/API 语义；仓库问题的证据始终是上述固定 SHA 源码，不能用官方文档替代实际生产接线。
