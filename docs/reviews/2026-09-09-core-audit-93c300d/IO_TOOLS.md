# I/O 与工具边界审查

基线：`93c300d9b222ea9720579b86ac273e945f1964bc`。本文件依据本轮已经完成的源码阅读与真实探针输出重建，**没有重新执行实验**。按最新范围，正文聚焦期限、资源回收、进程状态和游标；日志重定向放入附录，不扩展网络安全或攻防审查。

原探针源码、构建目录、Windows 数据和最初报告位于 `D:\Users\Ye_Luo\APP\context-agent-prototype\target\review-2026-09-09\io-tools\`。重建时确认整个 `target` 已不在磁盘，原因未确认；原源码与这些工件现不可打开，以下结果来自**当时的工具执行记录**。WSL `/tmp` 数据是否仍存在未重新检查。本轮没有修改生产文件；重建前 HEAD 不变，`git status --short` 仅有用户的 `?? .trae/`。

发现编号沿用原记录：正文四项，附录一项。这些是实际库调用证据，不是全仓验收或完整 Runtime/GUI 端到端测试。

## IO-03 · [P1] Supervision 锁退避没有累计期限

定位：[tool-runtime/src/supervision.rs:84](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/supervision.rs:84)，问题段 **79–91**。

更新式 `w[n+1] = min(2*w[n], 250)` 保证 `w[n] <= 250`，而退出条件要求 `w[n] > 2000`，因此超时分支永远不可达。单次等待有上界不代表总等待时间有上界。

实际探针以真实 OS 文件锁占住 `host-children.lock`，另一个线程调用 `reconcile_children`。Windows 和 Linux 均在 **4 秒**时仍未返回；释放锁后才完成，分别耗时 **4132 ms / 4131 ms**。当时已释放锁、join 自有线程，没有持锁残留。

调用方包括 [agent-compose/src/lib.rs:527](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-compose/src/lib.rs:527) 的启动对账，以及 [tool-runtime/src/tools/process.rs:995](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/process.rs:995) 的已 spawn 子进程登记。后者的执行 timeout 到 `process.rs:1104` 才建立，阻塞的同步登记也不检查取消 token。`lock_ledger` 同时持有进程级 `LEDGER_MUTATION`，其他台账操作可能一起等待。

建议用独立 `Instant` 截止时间或累计等待量控制总期限，保留有界单次退避；到期返回现有 typed lock 错误。已验证真实锁等待不收敛，未运行完整宿主启动挂起实验。

## IO-04 · [P2] 失败的 session start 泄漏 Pending 槽

定位：[tool-runtime/src/tools/session.rs:336](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/session.rs:336)，主要问题段 **336–341**；预留发生于 **323**。预留之后的 authority binding `?`（**372–378**）、seal refusal（**383**）、spawn `?`（**428–430**）也没有统一撤销该预留。

数量不变量应为 `slots = pending + running`，失败 start 返回后 `Δslots = 0`。当前失败路径留下 `Δslots = +1`，且没有向调用者返回可以 stop 的 session ID。

真实 `BuiltinToolDispatcher` 探针在同一空工作区发起 16 次不同不存在程序的 start，均返回 `program_not_found`；第 17 次改用真实、可直接退出的探针可执行文件，仍得到：

```text
InvalidRequest("process.session is limited to 16 concurrent sessions")
```

Windows、Linux 均复现；此前没有成功启动任何会话。随后 `shutdown_sessions` 清掉 Pending 并返回 `Ok(())`。这会让长期使用中累计的普通启动失败耗尽会话能力。

建议将纯预检移到占槽前，并为占槽到交接 Running 的所有 return、`?`、future drop 路径设置统一预留回收，不只修一个 `program_not_found` 分支。

## IO-05 · [P1] Session 输出 EOF 被误用为进程退出，poll 不兑现取消

定位：[tool-runtime/src/tools/session.rs:81](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/session.rs:81)，**81–88** 将输出通道断开置为 `exited=true`；随后 [session.rs:555](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/tools/session.rs:555) 的 **555–561** 无期限执行 `child.wait().await`。poll 分发 **239** 没有传入 cancel；全会话表锁从 **541** 持有到操作结束。

状态机应分别记录 `output = Open/EOF` 和 `process = Running/Exited/Unknown`。`output=EOF` 不能推出 `process=Exited`。当前子进程只关闭 stdout/stderr 而继续运行，就能让 poll 进入无界 wait。

Windows、Linux 真实子进程探针均观察到：

```text
session_poll_after_2s=Err(Elapsed(())) cancel=true
session_child_before_stop=Ok(Running(...))
session_stop=Ok(...)
session_child_after_stop=Ok(Exited)
```

取消发生于 poll 开始后 **200 ms**；2 秒后 OS 检查仍为 Running。探针结束外层测试 future 后调用正式 session stop，收到 Ok，并确认子进程 Exited（Windows pid **24144**，Linux pid **738**）。当时没有遗留这些测试子进程。

外层调用核对：[BuiltinToolDispatcher::execute:731](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/tool-runtime/src/registry.rs:731) 直接 await handler；[Core kernel:1259](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-core/src/kernel/mod.rs:1259) 直接 await dispatcher；[actor/tools.rs:625](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-runtime/src/actor/tools.rs:625) 将其置于独立任务。该执行链没有额外的 dispatch 总 timeout。

**范围限制：Actor 仍能接收 Cancel。** 命令循环见 [actor/mod.rs:1573](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-runtime/src/actor/mod.rs:1573)；停机对工具清理有独立 **5 秒**期限，超时返回 RecoveryRequired（**1683–1709**）。因此已确认的是 poll 本身不响应取消、持会话表锁并阻止正常会话清理收敛，不是“整宿主无法接收取消命令”。

建议仅用 `try_wait` 的明确结果判断退出，单独保存输出 EOF；活进程的 poll 不应无界等待。保留取消与总 drain 截止时间，并缩短全局会话表锁跨 await 的范围。

## IO-02 · [P2] `after_tx` 游标不排他，最新提交被重复返回

定位：[agent-workspace/src/lib.rs:1437](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-workspace/src/lib.rs:1437)，问题段 **1437–1443**；协议排他游标说明见 [agent-platform-protocol/src/work.rs:645](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-platform-protocol/src/work.rs:645)。

一次正常写入产生同一 `tx_id` 的 Prepared、Committed 两条记录。`read_changes(100, None)` 最新记录是 Committed；随后用这个最新记录的 `tx_id` 调用 `read_changes(100, Some(tx_id))`，Windows、Linux 都再次返回同一 Committed，期间没有任何新写入。

原因是扫描遇到第一次同 ID 的 Prepared 即开启 `after_cursor`，之后把同 ID 的 Committed 当作新记录。事务标识不是唯一记录位置，不能保证游标严格前进。客户端不能仅凭此游标确认已追平，连续拉取会重复得到旧提交。

实际产品入口是 B3 `work.changes` → [agent-runtime/src/platform/work.rs:733](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-runtime/src/platform/work.rs:733) → `Workspace::read_changes`。完整 UI 重复显示路径未实验。建议提供明确的单调记录位置，或先定义事务聚合后的游标语义；无需新建状态数据库。

## 附录 IO-01 · [P1] Change journal 可被重定向到工作区外文件

此项保留已取得的数据完整性证据，不作为本轮网络安全扩展任务。

定位：[agent-workspace/src/lib.rs:1373](D:/Users/Ye_Luo/APP/context-agent-prototype/crates/agent-workspace/src/lib.rs:1373)，写入问题段 **1373–1380**；读取同类问题在 **1418–1422**。`Workspace::open` 的 **445–547** 检查并通过句柄打开状态目录及 authority 日志，但没有验证或固定 `changes.jsonl` 的最终文件句柄。

探针在 **Workspace::open 成功之后**，于真实 `.focus-agent` 目录中置入 `changes.jsonl` 符号链接，指向当前宿主账户可读写的工作区外测试文件。WSL Linux 原生文件系统上，真实 `read_changes` 读出外部独有的 `outside-data-marker`；真实 `begin_mutation(...).apply(...)` 仅向工作区内 `normal.txt` 写入 12 字节，却令外部日志文件从 **113 字节增长到 478 字节**，追加 Prepared、Committed 两行。

所需路径约束是每次实际打开的 journal 对象属于受控状态目录；父目录曾通过验证，不能保证之后按路径打开的最终文件仍满足该约束。建议复用 pinned 状态目录及拒绝链接、非普通文件的 confined 最终打开。

前提与限制：必须有机会在状态目录放置或替换这个链接；未声称普通 `fs.write` 可以写入 `.focus-agent`。Windows 创建符号链接失败（`os error 1314`），本项 Windows 动态验证 **NOT_RUN**。**打开工作区之前预植链接的探针 NOT_RUN**；源码中未见启动时检查该最终文件，但不把这个推断写成已运行结果。

## 当时执行的命令与结果来源

原独立探针直接调用真实 Workspace、BuiltinToolDispatcher、supervision API。effectful dispatcher 请求使用与现有测试同结构的有效 effect identity，未经过完整 Core 审批、Runtime、宿主或 GUI 链。源码原位置为 `D:\Users\Ye_Luo\APP\context-agent-prototype\target\review-2026-09-09\io-tools\src\main.rs`，现已不在磁盘。

```text
# Windows；cwd 为原 target/review-2026-09-09/io-tools
cargo run --offline --manifest-path Cargo.toml --target-dir build

# WSL 独立构建：首次 offline 因缺 thiserror 2.0.20 失败
wsl.exe -d Ubuntu --cd /mnt/d/Users/Ye_Luo/APP/context-agent-prototype/target/review-2026-09-09/io-tools --exec /home/ye_luo/.cargo/bin/cargo run --offline --manifest-path Cargo.toml --target-dir linux-build

# 下载 thiserror/thiserror-impl 后构建成功
wsl.exe -d Ubuntu --cd /mnt/d/Users/Ye_Luo/APP/context-agent-prototype/target/review-2026-09-09/io-tools --exec /home/ye_luo/.cargo/bin/cargo run --manifest-path Cargo.toml --target-dir linux-build

# 原生 Linux 数据目录，复用当时构建的同一二进制
wsl.exe -d Ubuntu --exec mkdir -p /tmp/context-agent-review-20260909-io-tools
wsl.exe -d Ubuntu --cd /tmp/context-agent-review-20260909-io-tools --exec /mnt/d/Users/Ye_Luo/APP/context-agent-prototype/target/review-2026-09-09/io-tools/linux-build/debug/review-io-tools-probe
```

Windows 完整探针运行完成，链接项因权限限制未执行。Linux 首次在 `/mnt/d` 上运行，在第一笔 mutation 的提交后校验返回 `Internal("mutation applied state unknown: atomic replace returned success but committed target verification failed: No such file or directory (os error 2)")`，未走到后续探针。切换 WSL 原生 `/tmp` 数据目录后五项流程全部完成；未把该挂载差异另报为产品缺陷。

当时 Windows 数据目录为 `D:\Users\Ye_Luo\APP\context-agent-prototype\target\review-2026-09-09\io-tools\probe-data\7aea089b-a739-4288-9452-80d92a95a858`，现随 target 消失。Linux 数据目录为 `/tmp/context-agent-review-20260909-io-tools/probe-data/1efd2fa0-e210-4e43-8cc4-d3a77c5e043d`，未重新检查存续状态。以上不存在的原工件不作为当前可下载的交付物。

## 覆盖与限制

已读当前 `AGENTS.md`、`CURRENT.md`、`NEXT_TASKS.md` 当前队列；结合当前代码定向确认 metadata 发布失败围栏、搜索 PARTIAL/有界读、session spawn 后 artifact 失败清理等既有实现，未重新立项已关闭旧修复。文件/函数索引检索不计作全文审查。

| 范围 | 当时阅读的文件和路径 |
|---|---|
| agent-workspace | `lib.rs`：打开/路径解析、已有文件快照、artifact 打开、文件 prepare/commit/rollback、change journal；`confined.rs`：根与子目录句柄打开、链接拒绝；`journal.rs`：authority 打开与锁；`process_journal.rs`：打开、spawn/exit、恢复对账、append；`remote_journal.rs`：打开、远端状态记录及 reconcile；`broker.rs`、`handles.rs`：生产路径。 |
| agent-storage | `lib.rs`：FileOperationJournal 打开、metadata 发布、append/compact；FileEventJournal 打开、后台写者、flush。未完整复核 WAL recover/fold。 |
| tool-runtime | `tools/search.rs`：生产路径；`tools/mod.rs`：目录 walk/有界读、process effect context；`tools/fs.rs`：list 与分页前段；`tools/process.rs`：execute_invocation 解析后的授权、spawn、台账、输出与退出；`tools/session.rs`：生产路径；`registry.rs`：构造、生命周期片段、dispatcher execute；`supervision.rs`：锁、台账读写片段、reconcile、lease。 |
| agent-process | `supervisor.rs`：生产路径；`host.rs`：配置、call/exchange、取消与 shutdown/reap 片段。 |
| agent-capability-process | `capability_host.rs`：manifest/config、start/stop、invoke 前段；`mcp.rs`：poison/reap、写读取消、invoke/restart/stop。未将尚在 N8 的默认产品接入或缺失支持声明列为新问题。 |
| Context 进程边界 | `context-contextcore/src/lib.rs`、`adapter.rs`：配置、转发、materialization 校验；`agent-context-service/src/lib.rs`：engine 路由和 serve_session/response 生产路径。 |
| 交叉调用方 | compose 启动对账；Runtime changes 路由及协议游标契约；Core execute await；Actor dispatch、命令循环及 shutdown drain。 |

未覆盖各分配 crate 的全部源码和测试，尤其是：workspace 目录事务完整实现、runtime_facts 与全部恢复解析器；process lifecycle/watchdog/landlock/integrity 全文及故障矩阵；tool edit/patch/git/shell/code/verify/proof_runner/python/task 完整正文；capability 全部隔离启动/协议帧路径；context wire/main 全部正文。

没有运行全仓测试、冻结 M15/LT-EVAL、现有 crate 全量回归或远端 CI；没有验证真实 provider；不外推为 B1/B2 正式平台支持验收。重建报告不构成新一次验证。
