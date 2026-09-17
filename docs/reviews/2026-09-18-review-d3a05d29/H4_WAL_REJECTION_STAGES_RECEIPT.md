# H4 回执 — 持久日志错误分层（拒绝阶段决定恢复许可）

**提交：`ea5cbe76`**（第十一批，基线 `d3a05d29`，审查：[REVIEW.md](REVIEW.md)）。唯一改动文件 `crates/agent-storage/src/lib.rs`（+522/−30）；实施环境 Windows＋cargo 1.97.1，红例先行。

## 分类语义（最终形状）

`append_operation_record`（现 1211 行）返回新内部类型 `Result<(), AppendFailure>`（enum 在 1157 行），`append_and_sync` 分派块（686–723）按失败阶段处理：

| 失败点 | 分类 | writer 结果 | 返回给调用方 |
|---|---|---|---|
| 序列化失败 / 帧超 16KiB / stat 失败 / 大小溢出 | `RejectedBeforeWrite { capacity: false }` | 健康，不写 `failed` | 原样上抛（`Storage`） |
| 投影大小超字节容量 | `RejectedBeforeWrite { capacity: true }` | 健康 | `RecoveryRequired`（提示保留「checkpoint/compaction is required」）；`append_and_sync` 内**恰好一次**压缩重试：压缩成功后按新基线重置 `record.seq`、重新 `validate_cached_record` 再试一次；仍容不下则以「still exceeds its byte capacity after compaction」显式拒绝，绝不循环；重试若落在写阶段失败则照常 fence |
| seek/write/flush/sync 失败（`persist_operation_frame`，1182） | `WriteFailed` | `fence_operation_writer`（1198）sticky 封禁，消息格式与原文一致 | `Storage("… failed permanently: …")` |

G4 的 `<base>.lock` 生命周期锁、WAL 格式、真实 256MiB/16KiB 生产门限均未动；压缩路径与恢复侧硬限仍用真实常量。测试额度走 `cfg(test)`/`cfg(not(test))` 成对 seam（仿既有 `SYNC_DIRECTORY_FAULT` 风格：`operation_journal_byte_capacity` 613/620、`INJECTED_WAL_BYTE_CAPACITY` 626、`WRITE_PHASE_FAULT` 632），未为绿测试改任何生产门限。

「任何 Err＝永久封禁」的消费点已逐一核对：crate 内 `recover()`、`authority_checkpoint_marker()`、`validate_authority_checkpoint_marker()`、`compact()`→`compact_locked` 健康门、记录上限压缩触发——行为仅在容量/帧拒绝路径改变；上游 `agent-core/src/operation.rs:806` 对任何 Err 统一 latch `RecoveryRequired`，容量错误变体从 Storage 变为 RecoveryRequired 后该分支不受影响（`cargo check -p agent-workspace` 通过，未越界改上层）。

## 回归（5 条新测试；seam guard 2818/2834）

1. `capacity_rejection_leaves_the_wal_untouched_and_the_writer_healthy`：64B 额度→拒绝；WAL 逐字节不变、旧 marker 仍验证通过、recover/marker/compact 同句柄全通、解除额度后同句柄 append 成功。
2. `capacity_rejection_compacts_once_and_the_bounded_retry_appends`：9 帧同 op、额度＝当前 WAL 大小→一次 append 内完成恰好一次压缩（generation 恰为 2）、重试落盘、重开状态一致。
3. `capacity_rejection_after_a_futile_compaction_stays_a_healthy_refusal`：64B 额度→压缩后仍拒绝（显式 "after compaction"）、旧 marker 仍是祖先、writer 健康、可继续 compact（gen3）与 append。
4. `partial_write_failure_still_fences_the_writer_until_recovery`（负对照）：撕尾注入→sticky 封禁 append/compact/recover/marker 全拒；重开修复撕尾恢复服务。
5. `sync_failure_still_fences_the_writer_until_recovery`（负对照）：sync 前注入→sticky 封禁、compact 被拒；重开以磁盘真相对账。

红→绿：新实现完成后临时把分派块改回「任何 Err 一律 fence」旧语义运行，3 个容量测试全部 FAILED 且错误恰为矛盾形态 `… failed permanently: recovery required: … hard limit; checkpoint/compaction is required`；两个 fence 负对照在旧语义下照常通过（守护 fence 不放松）。随后从仓库外备份恢复（diff 逐字节一致），全量转绿。

## 命令与结果（实际执行）

- `cargo test -p agent-storage --lib`：**35 passed / 0 failed**（30 既有含 G4 锁回归＋5 新增；集成人复跑 4.38s 同结果）。
- `cargo clippy -p agent-storage --all-targets -- -D warnings`：通过；`cargo fmt -p agent-storage` 已执行；`cargo check -p agent-workspace` 通过。
- 集成终验：fmt --all --check 干净、五 crate clippy 干净、compose/conformance/workspace 全套 0 失败。

## 剩余限制（如实）

- 压缩重试中 `compact_locked` 自身失败（如候选路径被占）原样上抛、writer 按既有语义处置；容量问题留待调用方再试。
- 空日志（last_seq=0）容量拒绝时的重试压缩是 no-op，等于立即显式拒绝——有界且正确，但未跳过这次无效压缩调用。
- `agent-core` 仍对任何 append 错误 latch `recovery_required`；「上游利用健康 writer 直接 compact 而非进入恢复」是后续独立决策，本轮未越界。
