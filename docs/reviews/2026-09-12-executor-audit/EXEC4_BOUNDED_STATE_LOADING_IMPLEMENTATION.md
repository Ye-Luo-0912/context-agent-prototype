# EXEC-4 实施回执：长状态载入与审阅在读入阶段有界（M18 B 线第四片）

基线：`685b6bbb` ＋ 既有未提交工作树。对应 [REPORT.md](REPORT.md) **E08（P2）**、[TASKS.md](TASKS.md) EXEC-4。工作树落地（未提交、未推送、未跑远端 CI）。

## 用户能获得什么

载入大/损坏的 checkpoint 或谱系文件、审阅长 trace 时，得到**结构化的接受或拒绝**——拒绝发生在读入阶段（内存占用有已说明的上界），而不是先吃下整个文件再失败；正常大小的文件行为逐字节不变。

## E08 复盘与三条入口的修复

**① TUI `/restore`（`agent-tui/src/session.rs`）**：原为 `tokio::fs::read` 整读后 decode。现提取 `read_checkpoint_bounded(path)`：**一次 open、同句柄 `take(MAX_CHECKPOINT_ARTIFACT_BYTES + 1)` 读入**——超过 checkpoint 工件上限的文件在进入内存前被拒绝（错误点名边界与上限字节数）；读句柄使「stat 后文件增长」以实际读到的字节为准。合法文件逐字节不变，摘要验证照旧由 `decode_checkpoint_bytes` 负责。

**② workspace 谱系读（`artifact_run_lineage`）**：原为整读后检查 `bytes.len()`。现**同句柄 `take(MAX_ARTIFACT_RUN_LINEAGE_BYTES + 1)` 读入**——8 KiB＋1 字节的内存上界；超界按既有语义 fail closed（空谱系，不放大行任何前代）。

**③ replay `run_summaries_from_files`**：原为 `read_to_string` 整读 trace。现改为 `BufReader` 流式逐行折叠：
- `next_bounded_line`（fill_buf/consume 实现，内存 O(窗口＋cap)，绝不 O(行)）：单行上限 `MAX_TRACE_LINE_BYTES = 1 MiB`，**换行边界精确保留**——超长行丢弃内容、计入 omitted，其后一行照常折叠（回归钉死：恰上限行折叠／超一字节行计 omitted／后继行不受影响）。
- 结果集合本身有预算：单文件最多 `MAX_SUMMARIES_PER_FILE = 64` 个 run 摘要，超出行计入 omitted；`RunTaskSummary.omitted_lines` 如实记录跳过行数——「这是有界视图，不是全流」成为可读事实。

## 回归（4 项新增）

1. `restore_read_refuses_an_oversized_file_at_the_bound`（TUI）：稀疏文件恰好上限 → 读入成功且字节数精确；超上限一字节 → 读入阶段拒绝、错误点名边界。
2. `an_oversized_lineage_file_fails_closed_at_the_read_bound`（workspace）：合法 JSON 填充把谱系推过 8 KiB → 前代工件回到 fail closed；恰在上限内的正常谱系照旧授权读取。
3. `bounded_lines_keep_the_boundary_across_an_oversized_line`（replay）：恰好上限的行折叠、超一字节的行计 omitted、**换行边界不被吞掉**（后继行照常折叠）。
4. `per_file_summary_budget_and_omissions_are_counted`（replay）：`MAX_SUMMARIES_PER_FILE+10` 个 run ＋ 1 个非信封行 → 恰 64 个摘要、omitted 计满。
- 正常行为保持：既有 replay/restore 测试全绿；「正常完整 checkpoint 按现有摘要验证」由既有 host_restore/instance 覆盖未动。

**实现注记（如实记录）**：`next_bounded_line` 初版在返回判定处重复计入了换行偏移，把恰好上限的行误判超限——由回归 3 抓出并修正（判定基于「行字节（不含换行）≤ cap」），这正是回归先行的价值。

## 实际执行的检查（本机 Windows，2026-09-12）

- `cargo test -p agent-replay`：**61 通过**（含新增 2）。
- `cargo test -p agent-workspace --lib`：**111 通过**（含新增 1）。
- `cargo test -p agent-tui --bin agent-tui`：**57 通过**（含新增 1）。
- `cargo test -p agent-runtime --lib`：**400 通过**。
- fmt/clippy（agent-replay/agent-tui/agent-workspace/agent-runtime，all-targets）：**0 警告、本片文件干净**（runtime/prompt.rs 的 fmt diff 属 A 线在飞文件，不属本片）。

## 边界与如实记录

- 三条已确认入口闭合；未证明有问题的其他文件 API 未迁移（按停止条件）。
- 恰好上限的 checkpoint 在 TUI 的接受由 `MAX_CHECKPOINT_ARTIFACT_BYTES` 既有语义定义；超界的错误信息点名上限字节数。
- replay 的多 run 拆分行为较旧实现更完整（旧实现 run id 翻转时只保留最新 run，新实现按 id 保留至 64 个摘要）——更好的审阅完备性，仍是流式有界。
- **树上并行域如实记录**：验证期间 A 线（CTX-2/3）与 C 线（COST-1/2）在共享文件活跃落码，`agent-contracts`/`context-simple` 数次编辑中间态；等待其落定后全部相关套件转绿。未提交、未推送、未跑远端 CI。
