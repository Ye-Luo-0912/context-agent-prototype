# G5 回执 — Windows containment 入口收敛（C0 同类残余）

提交：`c8b0f3b3`。任务规格：[NEXT_ACTIONS.md](NEXT_ACTIONS.md) G5；缺陷分析：[REVIEW.md](REVIEW.md) 第 6 节；既有修复模式：[C0 回执](../2026-09-16-review-71f8a586/C0_PROOF_SUPERVISION_RECEIPT.md)。

## 范围声明（保持）

C0 已修复 tool-runtime proof runner（挂起创建→入 Job→确认恢复，fail-closed），本片**复用该模式**、不另写一份分歧实现；Core 审批/EffectIntent/权限语义不进 runner；不因内层关联失败断言整棵进程树必然失控（拒绝只说明配置的内层 containment 未建立，外层 Job 可能仍在约束）。

## 实现

- 新增 `agent-process/src/contained_spawn.rs` —— 两条生产路径共用的唯一入口：`CREATE_SUSPENDED` 创建（`ContainedCommand` trait，std/tokio Command 双实现）→ 必需 Job 关联经 child 自有内核句柄在**目标代码运行前**完成 → C0 同款 `resume_suspended_process`（ToolHelp 走查；windows-sys 0.59 该 feature 受门控故本地 unsafe extern 声明）→ fail-closed `recover()`（kill＋5 秒有界死亡确认；child 从未运行因此无后代）。类型化 `ContainedSpawnError`/`ContainmentFailure`。只负责创建/关联/恢复/生命周期。
- `host.rs::ProcessHost::connect` 经该入口 spawn；旧"普通 spawn 后分配、拒绝则降级"块替换为 fail-closed 类型化拒绝（死亡已确认→`Context`；未确认→`RecoveryRequired`）。attestation 证据字符串改为"assigned before the child's first instruction"——`job.is_some()` 现在意味着"首指令前关联已确认＋恢复已确认"，不再意味着"创建了 Job 对象"。
- `integrity.rs::run_wrap` 经同一入口；`let _ = assign_pid_to_job(...)` 与该函数一并移除。
- 测试 seam（最小、test-only）：env `AGENT_PROCESS_TEST_FORCE_JOB_ASSIGN_FAILURE` 每 spawn 读一次，把 job 句柄换成 null，经**真实生产入口**确定性触发内核拒绝。
- 测试 fixture（`bin/sandbox_probe.rs`/`bin/mock_host.rs`）：`tree <dir>`/`park` 模式（启动即派生后代、发布 pid、停车）＋`MOCK_DESCENDANT_PIDFILE` 钩子。

## 回归（真实 Windows 执行，身份 token/PID 重用安全检查沿 C0 模式）

单测：挂起 child 仅在确认恢复后运行（C0 契约移植）；null job 句柄→逐步命名的类型化错误＋死亡确认；正常路径控制（exit 42）；入口级宿主死亡（关闭唯一 Job 句柄杀死 child＋即时刻意派生的后代）。

集成（`tests/containment.rs` 四例，覆盖每个真实生产入口）：host 后代随宿主死亡＋正常握手/ping 控制；host 强制关联失败→无 runnable child（pidfile 不出现）；wrap 后代随 wrap 死亡；wrap 强制关联失败→exit 1＋类型化 stderr＋无 pidfile。

**红证据**（临时未提交注入 300ms spawn→assign 延迟进旧代码，C0 方法论；实验后移除）：3 例 containment 测试全红——host：`the descendant born at child startup (pid 79428) survived the containment kill`；wrap：`the wrapped child (pid 67740) survived`（kill 抢在延迟关联之前完成的双存活变体，即 C0 回执记载的快机变体）；host 强制失败例红因旧代码无 fail-closed 路径。泄漏存活进程需手动 kill——确认真实存活。自然（未注入）窗口在本低载机不复现，与 C0 相同。强制失败契约测试对旧代码结构性不可红（seam/检查在旧代码不存在；旧代码 wrap 场景永久停车）。

## 已执行验证（Windows 11 10.0.26340，cargo 1.97.1，合树后集成复跑）

- `cargo test -p agent-process`：**86 passed / 0 failed**（lib 43＝39＋4 新增、containment 4、host 28、integrity 5、sandbox 6）。
- `cargo test -p agent-capability-process`：**58 passed / 0 failed**（31/26/1；该 crate 无需改动）。
- `cargo clippy -p agent-process / -p agent-capability-process --all-targets -- -D warnings`：干净；fmt 干净。
- `cargo check -p tool-runtime`：Finished（C0 参考文件未动，对新 agent-process 编译通过）。
- 两个 fixture 后代 `zombie_processes`、两个 async 测试 env-seam `await_holding_lock` 的 allow 均有现场理由注释。

## 行为变化与边界（如实）

- **行为变化**：必需 Job 关联被拒时 fail-closed 取代旧降级。外层 Job 拒绝嵌套的宿主上，quota'd 配置的 `ProcessHost::connect` 现在报错而不是无 quota 层运行。tool-runtime 的 C0 路径仍是降级语义——两 crate 语义分歧已记录，交集成人后续决定是否统一。
- `resume_suspended_process`＋ToolHelp 声明在 tool-runtime（`pub(super)` 不可达）与本 crate 重复——已标记给集成人。
- 恢复确认是有界 5 秒阻塞轮询，仅达罕见失败路径。
- `agent-compose` proof_supervision 不由本片运行（集成段统一跑）；CI Windows 分片仍是最终权威。
