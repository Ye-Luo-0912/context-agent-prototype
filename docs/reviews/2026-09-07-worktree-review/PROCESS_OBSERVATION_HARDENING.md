# B1：进程状态与清理确认强化

2026-09-07。基线为 `92f8d92af93f7478ca1a5c1de11c38519e46c6a5` 加已有工作树修改，承接 [上一轮修复与设计复核](REMEDIATION_AND_DESIGN.md)。本轮交付一个功能切片：宿主无法核实进程状态时，保留监督责任并阻止恢复；只有明确的退出证据才能解除该项清理阻塞。修改留在工作区，未提交或发布。

## 已复现的问题

Windows 原实现将 `GetExitCodeProcess == STILL_ACTIVE(259)` 当作存活证明，将 `OpenProcess` 失败当作进程退出。两者都能在本机稳定复现。

| 本机探针 | 修复前 | 修复后 |
|---|---|---|
| 自建 `cmd /C exit 259`，等待结束并保留 Child 句柄 | `code=Some(259)`，`process_is_running=true` | `process_is_running=false` |
| 只读观察受保护的 System PID 4 | 创建身份读取失败，但 `process_is_running=false` | 观测返回 access denied；兼容谓词保守返回 true，不能据此确认退出 |
| 临时台账记录 PID 4，显式使用空身份令牌 | `cleared_stale=[4]`，台账被清空 | `unverified=[4]`，保留台账并阻止工作区复用 |

探针命令：

```text
cargo run --manifest-path target/strengthen-probes/process-state/Cargo.toml --target-dir target --offline --quiet
```

探针位于被忽略的 `target/` 下。System 进程只作只读观察，台账始终使用空身份令牌，任何分支均不得授权向它发送终止信号。实际终止测试只使用测试自行创建的子进程。

Windows 的进程终止应通过进程对象进入 signaled 状态确认；进程对象可因未关闭的句柄继续存在，退出码 259 也不能单独作为存活证明。依据：[GetExitCodeProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-getexitcodeprocess)、[Terminating a Process](https://learn.microsoft.com/en-us/windows/win32/procthread/terminating-a-process)。受保护进程的打开失败保留为观测错误，参见 [OpenProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-openprocess)。

## 实现与设计

共享进程层提供 `inspect_process -> Result<ProcessState, String>`，明确区分 `Running(identity)`、`Exited` 与无法观测。Windows 在同一个 RAII 句柄上查询等待状态和创建时间，句柄在所有退出路径自动释放。Linux 从同一份 `/proc/<pid>/stat` 读取状态与创建时间；zombie 可确认退出，读取失败时只有 OS 返回 ESRCH 才确认不存在。`u32` PID 转换前检查范围，避免越界数值变成负进程组编号。

终止接口返回 `AlreadyExited`、`IdentityMismatch`、`ExitConfirmed` 或 `Unconfirmed`。信号前无法观测、缺少身份或身份不符时不发送信号；信号后读取失败保留为无法确认。明确退出和身份复用证据才允许解除原进程的监督责任。旧的布尔 API 保留兼容，但恢复与台账使用类型化结果，避免通过布尔取反吞掉错误。

监督台账的 `ChildLease::Drop` 与启动对账共用该观测语义。状态不可读的记录进入 `unverified`，发出终止请求但未确认退出的记录进入 `unconfirmed`，二者均保留。正常结束和确定过期的记录继续沿既有耐久台账流程释放，不新增状态存储。

进程 effect 日志也使用同一清理结果。无身份的活动孤儿、权限不足及清理未确认返回 `RecoveryRequired`；恢复说明保留实际不确定性。只有已有的耐久 `Exited` 日志才能产生 `CompletedValue`，单凭 OS 已退出仍返回 `Ambiguous`。清理证据与副作用完成证据各自沿现有契约流转，恢复记录不产生重放授权。

| 文件 | 本轮改动 |
|---|---|
| `crates/agent-process/src/lifecycle.rs` | 类型化观测与清理结果，Windows 同句柄等待状态，Linux zombie/权限/PID 范围处理，以及对应回归测试 |
| `crates/agent-process/src/lib.rs` | 导出共享观测与清理接口 |
| `crates/agent-process/src/host.rs` | Unix 终止入口拒绝超出有符号 PID 范围的输入 |
| `crates/tool-runtime/src/supervision.rs` | 对账与 Drop 只接受明确退出证据，未确认记录保留 |
| `crates/agent-workspace/src/process_journal.rs` | 观测不确定时阻止孤儿恢复，准确返回 Ambiguous |
| `crates/agent-conformance/tests/dependency_boundaries.rs` | 更新既有 process_journal 允许导入的符号，crate 依赖方向不变 |

## 本轮实际验证

Windows 命令均在仓库根目录执行。相关编译检查随后由新增回归测试再次覆盖；没有重复运行全工作区测试。

| 命令 | 结果 |
|---|---|
| `cargo check --offline -p agent-process -p agent-workspace -p tool-runtime --all-targets` | 通过 |
| `cargo test --offline -p agent-process --lib lifecycle::tests -- --quiet` | 7 通过；下方全库结果已包含，不重复计数 |
| `cargo test --offline -p agent-process --lib -- --quiet --test-threads=2` | 35 通过 |
| `cargo test --offline -p agent-workspace --lib process_journal::tests -- --quiet --test-threads=2` | 8 通过 |
| `cargo test --offline -p tool-runtime --lib supervision::tests -- --quiet --test-threads=2` | 17 通过 |
| `cargo test --offline -p tool-runtime --lib python::tests -- --quiet --test-threads=2` | 6 通过，1 项原有 ignored |
| `cargo test --offline -p agent-conformance --test dependency_boundaries -- --quiet` | 3 通过 |
| `cargo test --offline -p agent-compose --test supervision_gate --test m16_restore -- --quiet --test-threads=2` | 监督门禁 3、恢复 2，通过 |

WSL Ubuntu 在 `/mnt/d/Users/Ye_Luo/APP/context-agent-prototype` 执行，使用单独的 Linux 构建目录：

| 命令 | 结果 |
|---|---|
| `cargo test --offline -p agent-process --lib lifecycle::tests --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 7 通过 |
| `cargo test --offline -p agent-workspace --lib process_journal::tests --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 7 通过 |
| `cargo test --offline -p tool-runtime --lib supervision::tests --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 17 通过 |

本轮 Windows 74、Linux 31，共 105 项测试通过；1 项原有 ignored。新增用例覆盖：退出码 259、受保护进程观测失败、信号前身份不可信、信号后观测失败、Linux zombie、越界 PID、无身份孤儿跨 Workspace 重开，以及无耐久完成记录时仍保持 Ambiguous。

捆绑 Python 执行 `scripts/doc_consistency.py`：通过，13 份活跃文档的链接与状态一致。本轮六份代码文件及 CURRENT/NEXT_TASKS 的 `git diff --check` 通过；全局检查的原有 `AGENTS.md:40` 文件尾空行不在本轮修改范围。

## 验收边界与下一切片

本轮完成进程状态与清理确认的功能切片，B1 整体仍在实施。实际验证覆盖 Windows 与 WSL Linux；其他 Unix 平台未实测，缺少可信创建身份时保守拒绝终止。

退出确认的轮询等待最多约 500 ms，但底层 `kill_process_tree` 的 Windows `taskkill` 调用未在本轮改为有界原生终止，不能把轮询期限表述成整个清理操作的总期限。创建身份检查与后续 PID/进程组发信号仍存在需要单独解决的竞态；组长退出也不能证明整个进程组已清理。

下一切片沿 B1 核对绑定原生身份的终止与整个进程组清理，再通过真实 Rust 宿主硬退出的 exact-proof 路径验收。当前测试不构成完整生产宿主硬退出、所有 PID 复用竞态或 B1/B2 正式支持声明。

本轮未修改客户端实现、GC/搜索默认策略，也未运行真实 provider、M15/LT-EVAL 或发布。RuntimeActor、Core 提交/恢复边界及现有监督配置归属保持不变。
