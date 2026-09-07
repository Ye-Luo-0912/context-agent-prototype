# 五条核心边界与宿主清理落地

2026-09-07。HEAD `92f8d92af93f7478ca1a5c1de11c38519e46c6a5` 加已有工作树修改。按用户确认的设计继续实施：保留 RuntimeActor、Core、可替换 ContextEngine 和现有存储/搜索体系。五条边界已有代码与回归覆盖，本轮补齐进程终止期间的身份持有、watchdog 进程组身份、session 清理以及未知退出结果的处理。

## 五条设计边界的实现对应

| 边界 | 已落实的行为 | 实现与回归入口 |
|---|---|---|
| Core / Runtime | RuntimeActor 统一编排；Core 产生审批结论、校验权限与 effect 身份并掌管提交/恢复。平台提交、审批、快照和订阅使用类型化数据。正文中的拒绝字样不产生审批权威，普通 final 不持久关闭任务。 | `agent-core/src/kernel/mod.rs` 的审批分支；`agent-runtime/src/actor/commands.rs` 与 `platform/work.rs`；`actual_approval_refusal_stamps_a_typed_no_dispatch_result`、actor 的 `work` / `work_control` 回归与依赖边界检查。 |
| 上下文 | attention、semantic、residency 各自记录。错误自动验证终结要求同一 TaskId 与同一宿主 VerificationProbe，后者包含配方 ID、修订及完整定义/覆盖声明摘要。驻留、外置和恢复后的待处理验证都遵守关联约束。 | `context-simple/src/gc/reachability.rs` 的 `queue_error_verifications`；`tests/lifecycle.rs` 的跨任务/检查修订/定义/恢复用例，以及旧待处理验证不能跳过核验的用例。 |
| GC | 工作集外置保持可逆；Storage GC 按保留规则和完整强引用闭包决定永久删除。StorageRequired 自身成为强引用遍历的根，保护传递证据；被保留的终态记录仍保持终态。 | `context-simple/src/store.rs` 的 `plan_storage_gc`；`storage_required_anchor_protects_its_transitive_evidence`、`storage_gc_strong_edge_closure_matches_manual_reachability`；`tests/admit.rs` 的存储保护与驻留分离用例。 |
| 搜索 | 保留 catalog / inverted index 与有界正文复核。文件数、目录项、累计路径、单次读取和返回片段均受预算限制；PARTIAL 与完整范围无命中分开，截取片段不冒充完整正文。 | `tool-runtime/src/tools/mod.rs` 的有界遍历；`tools/search.rs` 的超长命中行、部分扫描、超大文件与取消回归；`context-simple/src/index/` 和 `tests/search.rs`。 |
| 恢复 | 观测错误、超时及未确认清理保持错误或未知状态；监督记录保留并可阻止恢复。完成需要可信耐久回执，OS 退出或恢复记录本身不授权重放。 | `agent-workspace/src/process_journal.rs`；`tool-runtime/src/supervision.rs`；本轮新增的 session 未确认停止回归和真实 Rust 宿主 exact-proof 硬退出回归。 |

以上路径均相对 `crates/`。前四项的主要修复与完整设计分析见 [上一轮修复报告](REMEDIATION_AND_DESIGN.md)；上一切片的观测错误复现见 [进程状态报告](PROCESS_OBSERVATION_HARDENING.md)。本轮对上下文生命周期、StorageRequired、Core 审批和平台工作入口继续做定向复核。没有重调 GC、引入向量库或另建状态权威。

## 本轮新增的功能

### Windows：持有创建身份直到终止确认

`terminate_matching_process_tree` 打开可查询、可等待、可终止的进程句柄后，在该句柄上验证创建令牌，并持有它贯穿树清理、原生 `TerminateProcess` 和退出确认。根进程退出后该句柄仍保住原进程对象及其 PID，消除了本接口在校验与终止之间重新打开根 PID 的窗口。相关 OS 语义依据：[Microsoft 对 PID 复用时点的说明](https://devblogs.microsoft.com/oldnewthing/20110107-00/?p=11803)、[TerminateProcess](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-terminateprocess)。

树清理 helper 从 OS 提供的系统目录解析绝对 `taskkill.exe` 路径，使用系统目录作为 cwd，清理环境并隐藏窗口。helper 等待上限为 2 秒，超时后终止并最多再等 250 ms；根进程退出轮询另有约 500 ms 上限。helper 启动失败、超时或失败退出返回 `Unconfirmed`，直接终止根进程不能把该失败改写成成功。底层 OS API 调用本身不因此获得硬实时保证，树遍历也不等于原子 Job 容器。

### Unix：watchdog 持续保住进程组身份

watchdog 在 exec 前加入受监督孩子的独立进程组。只要 watchdog 仍在，该组就持续存在；组长被 reap 后也无需凭旧 PID 或 `/proc` 扫描猜测组的归属。收到管道 EOF 时只对自身所在的受监督组发信号，连同 watchdog 自身一起终止；普通退出由父进程有界回收 watchdog。`Interrupted` 读取会重试。

入口拒绝 0、1、超出有符号 PID 范围的值、宿主自身进程组，以及不满足孩子组长约束的目标。re-exec marker 必须匹配 watchdog 实际所属的组；外来编号不能让它终止另一组。组成员关系和 exec 继承语义见 [setpgid / getpgrp 文档](https://www.man7.org/linux/man-pages/man2/getpgrp.2.html)。

### session：失败路径也保有整树清理责任

Windows 工具库回归实际发现 `artifact_failure_after_spawn_releases_the_reservation` 留下后台进程；单独重跑同样失败。`process.session` 现在复用现有 Windows Job 围栏：spawn 前创建，spawn 后立即分配，guard 和正式 session 均持有 Job；启动失败、停止、销毁时释放容器。修复后 session 回归与整个工具库均通过。

`process.run` 的取消、超时、台账失败清理，以及 session 的放弃/停止，使用先请求终止、再有界等待的流程。`process.run` 的相关退出等待上限为 5 秒，session 为 1 秒。等待失败或超时传播 `RecoveryRequired`。session 只有拿到真实 `ExitStatus` 才能写耐久退出记录；新增故障注入回归证明，缺失等待观察时，停止失败且后续 effect 对账保持 `Ambiguous`。

### 真实 Rust 宿主 exact-proof 路径

扩展既有 `crash_child` 验收程序，启动真实 `RecipeProofRunner::verify_exact` 和它的工作子进程/后台成员。测试等待成员产生心跳后，用 OS 强制结束 Rust 宿主，确保不运行宿主 Drop，再确认整个测试树退出、没有生成 proof 结果，监督对账只释放已确认过期的记录。

该验收在 Windows 和 WSL Linux 都已通过，测试仅使用自行创建的进程与临时工作区，不调用真实 provider。它验证的是当前 exact-proof 执行器和原生监督机制；正式 P3 宿主、IPC 和 GUI 链路仍按各自工单验收。

## 实际验证记录

Windows 在仓库根目录运行：

| 命令 | 结果 |
|---|---|
| `cargo test --offline -p agent-process --lib -- --quiet --test-threads=2` | 38 通过。 |
| `cargo test --offline -p tool-runtime --lib -- --quiet --test-threads=2` | session Job 修复后 246 通过，1 项原有 ignored；随后新增未知退出回归，另行复验 session 全部用例。 |
| `cargo test --offline -p tool-runtime --lib tools::session::tests -- --quiet --test-threads=2` | 最终 14 通过，包含未知退出回归。 |
| `cargo test --offline -p agent-compose --test proof_supervision --test supervision_gate --test m16_restore -- --quiet --test-threads=2` | 硬退出 1、监督门禁 3、恢复 2，通过。 |
| `cargo test --offline -p agent-workspace --lib process_journal::tests -- --quiet --test-threads=2` | 8 通过。 |
| `cargo test --offline -p agent-conformance --test dependency_boundaries -- --quiet` | 3 通过。 |
| `cargo test --offline -p context-simple --lib tests::lifecycle -- --quiet --test-threads=2` | 20 通过。 |
| `cargo test --offline -p context-simple --lib storage_required -- --quiet --test-threads=2` | 3 通过。 |
| `cargo test --offline -p agent-core --lib actual_approval_refusal_stamps_a_typed_no_dispatch_result -- --quiet` | 1 通过。 |
| `cargo test --offline -p agent-runtime --test actor work -- --quiet --test-threads=2` | 18 通过。 |
| `cargo test --offline -p agent-compose --test proof_supervision -- --quiet` | 最终单独复验 1 通过。 |
| `cargo check --offline -p agent-process -p agent-workspace -p tool-runtime -p agent-compose --all-targets` | 最终相关 crate 的所有目标检查通过。 |

Linux 命令在 WSL Ubuntu 的仓库目录执行，内核为 `6.6.87.2-microsoft-standard-WSL2`：

| 命令 | 结果 |
|---|---|
| `cargo test --offline -p agent-process --lib watchdog::tests --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 6 通过；原来在线程中模拟组终止的场景已迁到下面的真实子进程测试。 |
| `cargo test --offline -p agent-process --test watchdog --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 4 通过：EOF 清理、组身份持续保留、组长回收后成员清理、外来/非法编号拒绝。 |
| `cargo test --offline -p tool-runtime --lib supervision::tests --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 17 通过。 |
| `cargo test --offline -p agent-workspace --lib process_journal::tests --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 7 通过。 |
| `cargo test --offline -p tool-runtime --lib tools::session::tests --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 最终 14 通过。 |
| `cargo test --offline -p tool-runtime --lib tools::process::tests --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 19 通过。 |
| `cargo test --offline -p agent-compose --test proof_supervision --target-dir /tmp/context-agent-strengthen-target -- --quiet --test-threads=2` | 1 通过；有界清理调整后再次通过。 |

初次 Windows 工具库的 session 失败已如实保留在上面的过程说明中，不计为通过。原始审查清单中的 8 份 .NET/Avalonia 文件再次核对 SHA-256，均未改变。本轮没有提交、发布、运行 M15/LT-EVAL 或更改冻结证据。

`agent-compose` 为既有验收二进制新增对 `agent-process` 的直接生产依赖，仍属于 composition → process-layer 的允许方向；没有新增 crate 或放宽 conformance 规则。相关代码的 `git diff --check` 通过；全局检查仍有原有 `AGENTS.md:40` 文件尾空行，本轮未改该文件。

捆绑 Python 执行 `scripts/doc_consistency.py` 通过：13 份活跃文档的链接与状态一致。

## 仍需单独验收的范围

B1 整体不在本轮宣告完成。正式平台宿主尚须验证同样的监督接线；spawn 到 Job/watchdog 就绪的窗口、OS 拒绝分配 Job/启用 watchdog、子进程主动脱离受监督组，以及遗留无容器孤儿的冷恢复，仍需分别处理与验收。Windows 的根身份持有不代表已证明 taskkill 对所有后代的遍历原子性；Unix watchdog 的组身份保证也不等于裸 PID 冷恢复已具备 pidfd 级保证。

下一切片仍沿 B1 的创建/监督就绪边界推进。Context、GC 和搜索在已有证据约束与预算下继续使用，算法研究等待明确的真实瓶颈。
