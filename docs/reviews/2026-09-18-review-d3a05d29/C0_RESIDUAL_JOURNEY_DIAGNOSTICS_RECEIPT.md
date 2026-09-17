# C0 残余回执 — t7 旅程等待的事件链最小诊断

**提交：`7b399497`**（第十一批，基线 `d3a05d29`，审查：[REVIEW.md](REVIEW.md)）。唯一改动文件 `crates/agent-host/tests/host_t7_journey.rs`（+404/−45，含 fmt 重排）；测试代码 only，生产代码零改动。

## 与上游收口的关系

C0 根因已由集成人在本批开工期间上游收口：run `35278745998` attempt 1 即已知 `wait_file_content` 30s 墙钟截止满载抖动第二次出现，`--failed` 重跑 attempt 2 全绿；等待截止沿 C1 先例钉宽 30s→120s（`b06892de`，fixture-only），钉宽已在 run `35282364426` attempt 2 的 CI 上通过。本片是其上的**事件链诊断层**，与钉宽正交：钉宽回答「等多久」，诊断回答「断在哪」；`b06892de` 的 120s 数值原样保留未动。

## 诊断机制（成功路径零输出，失败时随报文整体打印）

`JourneyTrace` 探针（相对时间戳＋Mutex 环，Arc 共享），五路事实来源：

1. **脚本模型侧**（provider 视角）：每轮 requested/emits（工具名＋call id＋path）/park 与释放/取消令牌收敛/plain final；
2. **wire 审批侧**：每次审批的 `request_id`、Allow、outcome=Delivered、序号与当时阶段；
3. **Runtime 事件侧**（第二个 broadcast 订阅的后台记录器）：FocusChanged、ModelStarted、**OperationAccepted**（tool/call/op/状态）、ToolStarted/ToolFinished（ok＋工具盖的 path）、AssistantMessage、Warning/Error/Failure/TurnFailed/TurnCancelled/TaskCompleted/RuntimeRestored、Lagged/Closed 如实记录；
4. **wire 里程碑**：submit/steer/cancel/checkpoint/restore；
5. **`wait_file_content` 失败证据**：轮询期首次观测（缺失 vs 内容不符）、截止时刻磁盘 exists/len、实际 vs 期望内容（160 字符截断）、`changes.jsonl` 按文件名的持久效果回执计数。对「证据读与截止的竞态」显式标注（landed between the deadline and this read — pure slowness）。

三个等待函数的失败分支均附链报告。禁令遵守：未预创建 part_a.md、未删改磁盘断言、未动截止数值、未忽略测试。

## 人为注入验证（验证后已完全还原，grep 无残留）

- **注入 A（内容不符）**：期望改 INJECTED-WRONG＋截止 1s → 报文含 `content: MISMATCH actual=… expected=…`、`change rows naming "part_a.md": 1`，链上可见 approval #1 request_id→Delivered、operation accepted、tool finished ok=true——即「效果已提交、内容/等待目标错」。
- **注入 B（文件缺失）**：路径改 injected_never_written.md → `disk: …MISSING (os error 2)`、`change rows: 0`，链上同时证明真实 part_a.md 效果已完成——与 A 形态明确可区分。
- 断链区分：若审批从未放行，链停在 `operation accepted/tool started` 且无 Delivered 行，`wait_turn_parked` 先以自己的链报告失败。

## 命令与结果（实际执行）

- `cargo test -p agent-host --test host_t7_journey named_pipe_t7_same_task_full_backend_journey -- --exact --nocapture`：9.41s 绿、9.53s 绿（与历史 ~9.5s 一致），成功输出与改造前逐字相同；还原注入后再跑 9.49s/9.51s 绿；transient 变体 4.55s 绿。集成终验 `cargo test -p agent-host` 全套 0 失败（集成人复跑）。
- `cargo fmt -p agent-host -- --check`：干净。

## 归因与剩余限制

- 归因：CI 失败与 30s 截止满载抖动高度一致（同 run 其余作业与同二进制另一 journey 全绿、attempt 2 全绿、本机稳定 9.5s、与 run 35103897272 同形态）——证据强度高但为排除性归因；本批之前无链级证据可复核当时断点，今后再现即有链报告可定位。无指向真实功能缺陷的信号（残余怀疑度低）。
- 事件记录器是最佳努力（broadcast 滞后记 Lagged，不保证零丢失）；审批未出现场景未做注入验证（需生产桩，超出测试-only 边界）；`broker-reservations.jsonl` 按文件名计数在本次观测为 0，仅参考证据，`changes.jsonl` 计数才是可靠回执。
- 120s 钉宽是否足够属集成人裁量，本批未改数值。本报告数字为本地 Windows 16 核实测，不外推 CI runner 时序。
