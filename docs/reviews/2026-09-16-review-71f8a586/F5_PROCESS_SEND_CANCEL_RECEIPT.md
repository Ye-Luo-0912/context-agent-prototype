# F5 落地回执：通用 ProcessHost 发送阶段的取消响应

- 基线：`dfbea60d`（ninth batch 开批提交，worktree 干净）
- 提交：
  - `d336e5ef`（`agent-process: stop-reading mock child mode exposes blocked frame writes`，测试夹具）
  - `e27d13bb`（`agent-process: bounded cancellable frame sends poison half-written pipes`，行为修复＋回归）
- 范围：`crates/agent-process/src/host.rs`、`session.rs`、`lib.rs`、`src/bin/mock_host.rs`、`tests/host.rs`
- 依据：[REVIEW.md](REVIEW.md) F5、[NEXT_ACTIONS.md](NEXT_ACTIONS.md) §3
- 未改动：`docs/CURRENT.md`、`docs/NEXT_TASKS.md`、MCP 客户端、Cargo.toml/Cargo.lock（无新依赖）

## 用户动作

子进程完成握手后不再读 stdin 时，取消仍能及时停止，不等 30 秒请求超时。首次请求写与 broker 答复写原先都是裸 `send_encoded_line(...).await`，管道背压把调用钉在写阶段，token 已取消也只能等外层 `request_timeout`（生产 `capability_host.rs` 配置 30s）。外层超时兜底存在，不是永久死锁，但发送阶段不享有读阶段同等的及时取消保证。

## 修复要点

| 位置 | 内容 |
|---|---|
| `session.rs::send_bounded`（新） | 唯一有界可取消发送入口：`write_all`+`flush` 与取消 token、剩余期限一起 `select`（biased，取消优先）。`Ok(())` 仅当完整写毕并 flush；`BoundedSendError::Abandoned` 如实表达"放弃时管道上可能有半帧"，调用方必须 poison＋终止，不得复用、不得重发；`Failed` 为 IO 错误。首请求、broker 答复、`op=cancel` 帧三处共用，无复制逻辑 |
| `host.rs::exchange_once` | 首请求写与 broker 答复写改走 `send_bounded`；broker 答复发送前新增取消检查（broker.handle 正常返回后、写答复前 token 已触发则直接结算，取消后不再有任何字节过线） |
| `host.rs::settle_abandoned_send`（新） | 放弃发送的归属判定：token 已触发 → `settle_cancelled`（`AgentError::Cancelled`，poison＋kill_tree，不对对端是否执行作断言）；否则按请求期限 → poison＋kill_tree＋"request timed out waiting for the frame write" |
| `host.rs::acquire_transport`（新） | 等待发送权（transport Mutex）纳入同一期限与取消：锁被在飞交换占住时，取消 → `Cancelled`（本调用未写任何字节，不 poison——在飞交换自行结算）；期限到 → 明确的放弃等待错误。锁空闲时 guard 分支优先，保持原有 poison 检查先于取消检查的顺序 |
| `host.rs` 期限贯通 | `call` / `call_with_cancel` / `call_with_cancel_and_broker` 入口计算一次 `deadline = now + request_timeout`，经 `exchange`/`exchange_response`/`exchange_once`/`send_bounded` 使用；外层 `timeout()` 原样保留作兜底。握手 `connect` 以 `startup_timeout` 同样贯通 |
| `mock_host.rs` | 新增 `MOCK_STOP_READING_AFTER_BYTES=<n>`：真实握手 ping 应答后，可选先发一条 `{"system":"fs.read"}`（`MOCK_EMIT_SYSTEM_FRAME=1`），再消费恰好 n 字节并把计数写入 `MOCK_READ_SNAPSHOT`，然后永久停读（进程存活直至被 kill）。快照即"部分写入确已开始"的子进程侧证据 |

半帧规则：取消/期限放弃发送时连接一律 poison＋kill_tree（`exchange_response` 内 reap），绝不复用、绝不自动重发副作用请求；结果归属只按本端事实（`Cancelled` 或期限超时），不报"对端未执行"。写开始前的取消（入口检查）不写任何字节，连接保持可用。

## 回归（红先，真实子进程，不 mock 管道语义）

`tests/host.rs` 新增 4 条（夹具为真实 `mock_host.exe` 子进程）：

| 测试 | 断言要点 | 修复前 | 修复后 |
|---|---|---|---|
| `cancel_stops_a_blocked_request_write_within_the_cancel_budget` | 子进程停读；256 KiB 请求（合同内，远超 OS 管道缓冲）写阻塞；快照证实子进程已消费 64 KiB（部分写入已开始）＋再等 300ms 调用仍未返回；取消 → <5s 返回 `Cancelled`（实测 ~0.55-0.69s，含 kill/reap）；连接 Quarantined；后续调用报 poisoned；心跳文件停跳（子进程树确被终止） | **FAILED**：取消后 10s 看门狗到期调用仍未结束（只会等 30s 请求超时） | ok |
| `cancel_stops_a_blocked_broker_answer_write_within_the_cancel_budget` | 同一楔死点移到 broker 答复：子进程发 system 帧后停读，broker 返回 256 KiB 答复卡写；同样证据链 → <5s `Cancelled`＋poison | **FAILED**（同上） | ok |
| `request_deadline_stops_a_blocked_write_and_poisons` | 无取消 token 时期限仍是边界：卡写 2s 期限结束、报 timed out、poison（实测 ~2.5s，期限 2s＋kill/reap） | ok（外层超时既有行为） | ok（守卫新代码路径） |
| `cancel_before_any_write_keeps_the_brokered_connection_usable` | 写开始前取消 → `Cancelled` 且连接 Ready，随后同一连接完成正常 exchange（broker 路径守卫） | ok（既有入口保证） | ok（守卫共享发送入口） |

证据链说明：取消前先取得快照（子进程已消费 64 KiB ⇒ 宿主写已交付 ≥64 KiB）再加 300ms"调用未返回"，而帧剩余 ≥192 KiB 大于任何默认管道缓冲，故写必然楔在写阶段而非读阶段；若在读阶段，修复前的读循环取消 select 本就会快速返回——修复前实测卡死本身即写阶段楔死的因果证据。

## 变异恢复法复验

每次变异后重跑关键用例，随后立即恢复：

- 变异 A：首请求写退回裸 `send_encoded_line`（等价修复前代码）→ `cancel_stops_a_blocked_request_write...` **FAILED**。
- 变异 B：`settle_abandoned_send` 取消分支只返回 `Cancelled`、不 poison 不 kill → 两个阻塞写用例 **FAILED**（Quarantined/poisoned 断言抓住）。
- 变异 C：取消被错报为期限超时 → 两个阻塞写用例 **FAILED**（`must surface as Cancelled` 断言抓住）。

## 已执行验证（真实命令）

```
cargo test -p agent-process --test host                    → 28 passed; 0 failed（修复前 26 passed + 2 FAILED）
cargo test -p agent-capability-process --test capability_process → 26 passed; 0 failed
cargo test -p agent-process                                → 全部通过（39 单元 + 28 host + 5 + 6 等，0 failed）
cargo test -p agent-conformance --test adapter_fault_matrix → 11 passed（FramedProtocolSession 其他使用方无回归）
cargo fmt → clean（--check 通过）
cargo clippy -p agent-process --all-targets → 0 警告
cargo clippy -p agent-capability-process --all-targets → 0 警告
```

稳定性：修复后全套件连跑 8 次 28/28 全绿。

负载相关性记录（按任务要求两种结果都记录）：验证期间出现过 2 次既有测试 `cancel_without_peer_ack_still_kills_after_the_bound`（断言 <2s）在共享 CPU 重载下偶发超界。将该既有测试在 HEAD 基线（stash 我的改动后）连跑 4 次全绿、我的版本下偶发失败对比后，确认是新增重例（真实子进程＋大帧）抬高了套件自身并行负载，而非修复的语义回归（该测试路径不含任何被改时序）。处置：重例载荷 2 MiB→256 KiB（仍必阻塞）＋三个阻塞写用例用静态槽串行化，处置后 8/8 全绿；既有测试断言未动。

## 取消预算实测

- 阻塞首请求写＋取消：~0.55-0.69s（对照 30s 请求超时；含 poison＋kill_tree＋reap，共享 CPU 下浮动）
- 阻塞 broker 答复写＋取消：~0.55-0.69s（同上）
- 期限路径（无取消）：~2.5s（2s 期限＋kill/reap）

## 限制（如实）

- 未跑：任何 paid/ignored 长任务；MCP cancel-during-write 用例属另一实现且已通过，本次未改 MCP、未重开其语义，其通过不作为本路径覆盖。
- `acquire_transport` 的锁等待期限分支（在飞交换占住锁时后来者的放弃等待）无直接测试：公共 API 的期限是宿主级配置，构造"后来者期限先于占锁者自行结算"需要扭曲夹具；该分支按构造保持"未写任何字节、不 poison、在飞交换自行结算"，属防御性收口。
- "取消分支在 biased select 中先于同次唤醒就绪的写结果"意味着取消瞬间即使帧已写完也按取消＋poison 结算（与读阶段 cancel-after-write 语义一致，保守方向）。
- 取消预算数值来自本机（16 核 Windows，与其他 agent 共享 CPU）；绝对值随负载浮动，测试断言的上界是 5s。
- 文档所述已执行验证均为本地实测；未在 CI 观察（合入后按既有 CI 执行）。
