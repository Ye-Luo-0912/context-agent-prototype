# 本轮执行与隔离探针

所有命令从 `D:\Users\Ye_Luo\APP\context-agent-prototype` 执行。探针仅写 `target/review-probes` 下独立目录；没有更改主仓库 Git 配置、产品源码、既有测试或用户数据。使用内存传输、合成模型和独立 Git 仓库，不调用 provider。

探针源码是本次审查的临时诊断文件，位于 gitignored 的 target 下，`cargo clean` 会删除它们：

- [Rust main.rs](D:/Users/Ye_Luo/APP/context-agent-prototype/target/review-probes/rust/src/main.rs)
- [Rust Cargo.toml](D:/Users/Ye_Luo/APP/context-agent-prototype/target/review-probes/rust/Cargo.toml)
- [.NET Program.cs](D:/Users/Ye_Luo/APP/context-agent-prototype/target/review-probes/dotnet/Program.cs)
- [.NET csproj](D:/Users/Ye_Luo/APP/context-agent-prototype/target/review-probes/dotnet/ReviewProbes.csproj)

## Rust 应用/Context 探针

实际命令：

```powershell
cargo run --manifest-path target/review-probes/rust/Cargo.toml --target-dir target --offline --quiet
```

实际输出：

```text
subscribe_gap: cursor=1, watermark=2, resync_required=false, replayed_event=false
retry_while_running: first=Accepted, retry=Err(InvalidRequest("agent is busy: a turn is already running"))
changed_recipe_and_task: old_errors_finalized=1
untrusted_error_text: gate_was_not_called, resulting_class=Some(ApprovalDenied)
```

分别使用实际 WorkControlRouter、新建 RuntimeActor/HangingModel、SimpleContextEngine 和 sanitize/diagnosis 公开函数。未修改被测实现。

## Git 工具探针

实际命令：

```powershell
cargo run --manifest-path target/review-probes/rust/Cargo.toml --target-dir target --offline --quiet -- --git
cargo run --manifest-path target/review-probes/rust/Cargo.toml --target-dir target --offline --quiet -- --git-driver
```

第一条在独立目录使用 `git init` 和 `git add` 建立 index，无需提交，也不改变主仓库。对比直接 Git 与 builtin 的大 diff 路径，得到：

```text
git_large_diff_direct: bytes=816129, success=true
git_large_diff_tool: error=tool error: git ["diff", "--", ".", ":(exclude).focus-agent", ":(exclude).focus-agent/**"] timed out, elapsed=20.059892s
```

外部 driver 的初版通过 `sh` 执行，在本轮初始沙箱环境中遇到 MSYS `CreateFileMapping ... Win32 error 5`，该次没有写出标记，不能作为“写成功”证据。之后把 driver 改成同一探针的 native executable，路径不含 shell 参数；程序只在探针 Git 目录内写标记。第二条实际输出：

```text
readonly_git_external_driver: approval=Allow, wrote_marker=true, outcome_ok=true
external_driver_result: ok=true,
```

该证据验证了真实只读策略和 builtin dispatcher，未使用 process.run/fs.write 的授权。复现夹具留在 `target/review-probes/git-*`，没有清理或修改主工作区文件。

## 损坏 scope 恢复探针

```powershell
cargo run --manifest-path target/review-probes/rust/Cargo.toml --target-dir target --offline --quiet -- --scope-cycle
```

```text
scope_cycle: restore accepted a task scope whose parent is itself
scope_cycle: completion did not return; watchdog exits isolated probe
```

将新建引擎 checkpoint 中一个 Task scope 的 parent 改成它自己的 id；restore 返回成功，TaskCompleted 进入无限后代遍历。为避免无限占用资源，探针独立线程在 250ms 后退出整个隔离进程。`cargo run` 返回失败；这是刻意观察卡住路径的结果，不是通过的测试。

## .NET 客户端与 ViewModel 探针

```powershell
dotnet run --project target/review-probes/dotnet/ReviewProbes.csproj --no-restore
```

```text
no_active_turn: JsonException: no_active_turn ack carries unknown fields
configured_identity: AgentContractViolationException: invalid protocol.identity: does not match the negotiated profile
notification_without_request_id: delivered=False, queue_completed=True, connection_healthy=True
continue_after_lost_receipt: applied_commands=2, connections=2, reported_task=10000000-0000-4000-8000-000000000001
approval_refresh_retention: before=6, after_100_snapshots_and_clear=206
```

continue 的服务端是内存传输：它在应用第一次命令后关闭输入而不回回执；SDK 重连后再次发送命令。计数器代表实际发送/应用的协议命令数，不代表真实工具副作用。ViewModel 探针加载本轮构建的 Agent.Desktop，停掉 timer 后重复应用审批快照，读取真实 command 集合数量，不打开用户界面窗口。

## 现有测试失败详情

`.NET` 现有测试为 22/24 通过，失败项：

1. `DeltaCoalescerTests.Append_coalesces_into_windows_and_flush_is_lossless`：`ResilienceTests.cs:165` 的 `Assert.Empty` 收到 `你好，`。实现初始 `_lastFlush=DateTimeOffset.MinValue`，首次 Append 立即 flush，与测试预期不一致。
2. `ResumableSessionTests.Faulted_connection_reconnects_rebuilds_and_reports`：第一次 Snapshot 操作尚包含后续 Subscribe，one-shot fixture 却在第一帧回复后关闭 listener；实际出现重连 `SocketException`，测试在首个 Snapshot 就失败。这个 fixture 不构成成功的重连验收。

Rust `--keep-going` 编译失败位置：

| 错误 | 位置 |
|---|---|
| 缺 `verify_recipe` | `agent-contracts/src/context.rs:2912`、`agent-contracts/src/discovery.rs:708` |
| 缺 `verify_recipe` | `agent-core/src/kernel/tests.rs:232`、`:287` |
| 缺 `verify_recipe` | `agent-runtime/src/prompt.rs:1084` |
| 缺 `Route` 导入 | `agent-runtime/src/platform/work.rs:685` |
| 私有 spawn_runtime 导入 | `agent-runtime/tests/actor/work_control.rs:22` |
| disposition 类型不匹配/未导入 | 同文件 `:110`、`:128` |
| broker/gate 移动后使用 | 同文件 `:240`、`:249` |
| `TaskId` 仍匹配 `()` | `agent-compose/tests/live_walk.rs:312`、`route_flow.rs:269` |

初次 GUI 构建因 Avalonia telemetry 尝试写沙箱外 `buildtasks.log` 失败；随后使用包自带 target 条件 `-p:UsedAvaloniaProducts=` 跳过 telemetry，构建成功。没有修改项目配置，也没有将初次失败归因于产品源码。

现有文档检查实际使用：

```powershell
& 'C:/Users/Ye_Luo/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/python.exe' scripts/doc_consistency.py
```

输出 `document-consistency gate: OK (13 live docs, links and state agree)`。系统的 `python` 是 WindowsApps alias，`py -3.12` 指向无法启动的位置；它们失败后才使用 bundled runtime。未据此修改系统 Python 配置。
