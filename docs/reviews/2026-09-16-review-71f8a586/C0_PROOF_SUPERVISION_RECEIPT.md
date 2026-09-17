# C0 回执：Windows CI 的验证进程树清理失败（已定位并修复）

分支 `c0-proof-supervision`，工作树起点 `dfbea60d`（审查基线 `71f8a586` 的第九批开批提交）。
调查与修复只落在 `crates/tool-runtime/src/tools/process.rs`。未改测试、未改 CI、未改 Cargo.toml/Cargo.lock。

## 根因结论（已由注入实验因果确认）

**Windows host-death containment 的 job 赋值发生在子进程 spawn 之后，两者之间存在窗口：被监督树中的进程在该窗口内出生就不会进入 Job。** 宿主被 TerminateProcess 杀死时，kernel 关闭 Job 句柄触发 `KILL_ON_JOB_CLOSE`，只杀已进入 Job 的 leader；窗口内出生的 member 不属于 Job，继续存活到自己的 30s 自退出。

完整链路（`crates/tool-runtime/src/tools/process.rs` 修复前）：

1. `execute_invocation` 在 spawn 前 `HostDeathJob::create()`（KILL_ON_JOB_CLOSE，句柄不继承）。
2. `command.spawn()` 启动 recipe 进程（即测试中的 leader）。
3. spawn 与 `job.assign(leader_pid)` 之间隔着 supervision ledger 的文件 I/O 与宿主线程调度；CI 上 Defender 扫描、慢盘与抢占可把这段拉长到与"新进程启动 + 它 spawn 第一个子进程"同量级（leader 与宿主是同一镜像，页面与扫描缓存全热，启动很快）。
4. leader（`crash_child` 的 `proof_worker`）先 spawn member 再写 `leader.pid`，所以测试读到任何 pid 文件时这个竞争早已定格，且不产生任何 stderr 痕迹（assign 拒绝才有降级标记）。
5. 宿主死亡 → Job 关闭 → leader 被杀（Exited），member 在 Job 外存活。与 CI run 35156892711 的失败签名逐项一致：panic 位于 `proof_supervision.rs:171`，`leader Ok(Exited)`，`member Ok(Running(...))` 且 identity_token 与捕获时相同（排除 PID 重用），host stderr 为空。

Linux 分片不受同窗口影响：其 containment 是 watchdog 进程持有的进程组，组员资格在进程创建时内核即刻继承，watchdog 触发时按 pgid 杀整组，晚于 watchdog arm 出生、早于 arm 出生、还是两者之间的成员都覆盖。

## 变异恢复法证据（承重验证）

临时在 spawn 与 assign 之间注入延迟（模拟 CI 上的宿主线程拖延；实验代码未提交）：

| 状态 | 注入 | 结果 |
| --- | --- | --- |
| 修复前 | 25ms | 3/3 红，签名与 CI 完全一致（leader `Ok(Exited)`、member 同 token `Ok(Running)`、stderr 无降级标记、同一 panic 行） |
| 修复前 | 50ms / 300ms | 红（本机过快，kill 先于 assign，呈现"双活"变体，机制同源） |
| 修复后 | 25ms | 3/3 绿 |
| 修复后 | 300ms（极端） | 2/2 绿 |

同一注入在修复前确定性转红、修复后转绿，证明测试捕捉的正是该缺陷、修复确实关死了该窗口。

## 修复内容（`crates/tool-runtime/src/tools/process.rs`）

- Windows 下 recipe 子进程以 `CREATE_SUSPENDED` 创建：挂起的进程一条指令都未执行，不可能在任何时点先于 assign 产出后代；member 因此必然出生于 Job 赋值之后并继承 Job 成员资格。
- assign 完成后才 resume 初始线程（新增 `host_death_job::resume_suspended_process`）：ToolHelp 线程快照按 owner pid 找线程，`OpenThread(THREAD_SUSPEND_RESUME)` + `ResumeThread`，成功以"恢复调用返回值 ≠ 失败标记"确认；快照对新进程可短暂竞争，带 3 次 × 10ms 有界重试。ToolHelp 三个入口是 kernel32 自 XP 起的稳定导出，在本文件内手工 `unsafe extern` 声明，避免为一次调用给既有 windows-sys 依赖增开 feature（Cargo.toml 不动）。
- fail-closed：无法确认 resume 时杀树、有界等待确认死亡（沿用 ledger 失败臂的同型处理），返回 `RecoveryRequired`/`Tool` 类型化错误，绝不留一个挂起到超时的子进程，也绝不在 assign 之前 resume。
- 超时预算不受影响：`deadline` 在 resume 之后才启动，子进程没有因挂起损失任何执行时间预算。
- 新增单元测试 `a_suspended_child_runs_only_after_the_confirmed_resume`：挂起 300ms 不可退出 → resume 成功 → prompt 退出，钉住该契约。

## 本地复现统计（诚实边界）

- 修复前原样运行 `cargo test -p agent-compose --test proof_supervision -- --test-threads=1`：10/10 绿（其中 5 次在 8 个并行 CPU 死循环负载下）。本机 16 核、宿主与 leader 同镜像全热，自然竞态未在本机自发出现；失败率证据来自远端（980bbc77 于 20:52 同测试 Windows 通过，71f8a586 于 22:26 失败，测试代码相同，测试二进制哈希相同 `proof_supervision-a1c33b26dc2c3b14.exe`，属间歇性竞态而非确定性缺陷）。
- 因果确认靠注入窗口（见上表），不是宣称本地自发复现。

## 已执行回归（真实命令与结果）

```
export CARGO_TARGET_DIR=/d/Users/Ye_Luo/APP/cap-agent-target-a
cargo test -p agent-compose --test proof_supervision -- --test-threads=1   # 修复后连跑 10 次：10/10 ok
cargo test -p tool-runtime --lib                                           # 279 passed; 0 failed; 1 ignored（既有 ignore）
cargo test -p agent-compose --test crash_resume --test supervision_gate    # 4 passed + 3 passed
cargo fmt -p tool-runtime && cargo clippy -p tool-runtime --all-targets    # 无告警
```

未跑：`cargo test -p agent-host`（改动不涉及 agent-host，NOT_RUN）；全 workspace（任务定义不要求，NOT_RUN）；tool-runtime lib 中既有的 1 个 ignored 未动。

## 剩余限制

- `agent-process`（MCP/capability host 的 `JobObject::create_job_object` + spawn 后 assign）与 `agent-process/src/integrity.rs` 存在同构的 spawn→assign 窗口，属同一缺陷类但不在本次失败路径上，未动（避免牵连无关面）。建议后续以同一 suspend→assign→resume 模式收口。
- `CREATE_SUSPENDED` 语义覆盖 `execute_invocation` 的所有 Windows spawn（process 工具与 host proof lane 共用该路径），行为差异仅是子进程晚几毫秒起步；tool-runtime 279 个库测试（大量真实子进程 spawn）全绿佐证无回归。
- 远端 CI 的最终确认以合入后的 Windows 分片为准；本地证据链为"注入转红/修复转绿 + 10 次全绿"，不以一次重跑绿宣称根因消失。

## 提交

- 代码：`2e70efc9` `tool-runtime: proof tree child is created suspended and resumed only inside the host-death job`
- 回执：本文件
