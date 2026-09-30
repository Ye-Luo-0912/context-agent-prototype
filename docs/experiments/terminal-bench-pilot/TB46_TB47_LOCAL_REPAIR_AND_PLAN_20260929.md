# TB46/TB47 本地修复与单变量窗口准备（2026-09-29）

本轮承接 [TB45 评估](TB45_ASSESSMENT_20260929.md) 和用户“继续，依次完成”。**本段记录开窗前已完成的本地修复、回归、两份 ELF 构建及零供应商预检。** 用户随后批准候选 Core grant 并启动 TB46；首槽非可比，后续槽按停止规则跳过。实际结果见 [TB46 中止回执](TB46_ABORTED_20260929.md)。

## 已完成的本地切片

1. **未知用量重新冻结。** `credential_relay.py` 的缺 usage 分支现在设置不可重试的 unknown 围栏。复现序列 `408 → 同体重试成功 → 新响应缺 usage → 再请求` 中，修复前状态码为 `[503, 200, 200, 200]`，修复后最后一条为 423、mock 上游只收三次请求，第一次 408 和后续缺 usage 的预约均保留。已有同体重试成功、二次 408 冻结回归仍通过。对部分流或其他非 408 未知结果的冻结规则不变。
2. **报告预算收尾。** `outcome.py` 从最后一个类型化 `tool_surface_planned` 的 `decision_budget_finalization` 原因及其后已结算的 `model_used` 推导 `runtime.finalization_reason`。TB45 两槽原始 events 复算均得到 `decision_budget_finalization`，同时旧字段 `round_budget=false` 原样保留；无该事件或只规划未执行时为 `null`，不读取 final 文字作状态依据。
3. **目标镜像 ABI 预检。** `run_bounded_b_pair.py` 在读取账户配置和创建 relay 前，用已核验的任务 image ID，断网、只读挂载 ELF 执行 `--help` 并检查 `--state-dir=`。启动失败只报类别，不回显 loader/容器原始输出。TB45 旧基线和新处理组均在固定 Terminal-Bench Debian 12 镜像中实测 PASS；这道门能在付费开窗前识别 TB44 的 glibc 类启动问题。
4. **Context 额度分摊。** Runtime 原先把 provider 额外发送窗口中保留的完整工具交换再次计入较小的 Context pack 窗口，长回合可把 `ContextQuery` 的可用额度压到零。现在只把超出 provider 专用 headroom 的 turn-frame token 计入 pack 预算，最终请求仍按完整 turn frame 受 provider 发送上限校验。

该 Context 改动的 56 决策零供应商回归：修复前，最终三轮 `ContextPrepared.selected=[]`；修复后，最后几轮重新选入 Context 条目。早期类型化失败仍是 Live，进入 Warm 后可通过 ContextEngine 公开接口按 ID 取回。**本次没有把所有未解决错误强行常驻模型提示。** 闭合工具作用域里的 Warm Error 是否应主动召回，需要单独的有界选择策略；TB45 第 18 轮权限报错其实是 `ok=true/exit=0` 的探针正文，第 54 轮才是实际进程失败，不能由这一条轨迹证明模型因 Context 遗忘而重犯。

## 本地验证

- Python `test_terminal_bench_outcome`、`test_terminal_bench_pilot`、`test_terminal_bench_relay`：43 项中 42 通过、1 项平台条件跳过。首次与 Rust fmt 并行时，既有请求间隔测试测得 47 ms、低于 60 ms 阈值；单项和整套串行复跑均通过。该 timing 观察保留，不把它写成 relay 代码故障已修。
- `cargo test -p agent-runtime --lib -- --test-threads=1`：452/452 通过。
- `cargo test -p agent-runtime --test turn -- --test-threads=1`：173 通过、1 项已有忽略，包含新的长回合回归。
- `cargo fmt --all -- --check`：通过。Windows Cargo 仍输出 finalized incremental 目录 GC 的 Access Denied 警告，未影响上述测试退出码。
- 离线 `rust:1.97-bookworm` 构建 `agent-tui` 后，两份 ELF 在固定题目 image ID `sha256:dbd2997e95d6bdde237f94cefb8cdc45ef5c7bbd44ecde02052a6dffcde14054` 的真实 ABI 预检均为 PASS。

## 待批准的最小真实试验

保持官方 `live-database-cutover` 固定题包、同一 `gpt-6-luna` Responses / reasoning `max`、同一 host relay、同一 Core grant 集合和所有既有试验上限。先运行 TB46 基线配对，再运行 TB47 处理配对；两个协议各含 Dynamic 和 Rolling，总计最多四个槽。二进制的唯一生产代码差异是上述 Runtime Context 额度分摊，Python relay/报告/ABI 预检代码在两个协议中相同。

| 身份 | ELF | SHA-256 | 预检 |
| --- | --- | --- | --- |
| `TB46_BUDGET_BASELINE_56_PAIR_20260929` | `target-debian12/experiments/tb45-budget-baseline/agent-tui` | `88bbf059d1ec56b9cf7abf097a6ab22222ddecde4635212f74e75e1c3bd5fb4d` | `STATIC_READY_NO_PROVIDER_CALLS`；目标镜像 ABI PASS |
| `TB47_BUDGET_TREATMENT_56_PAIR_20260929` | `target-debian12/x86_64-unknown-linux-gnu/debug/agent-tui` | `119ba505c31f2444d6674980b62af0d052041ab57e4009ceceff97db568bb262` | `STATIC_READY_NO_PROVIDER_CALLS`；目标镜像 ABI PASS |

每槽最多 56 决策、70 次上游尝试、8M input、440k output、1400 工具尝试、8400 秒；relay 仍有每槽 `$4` 的**直接 API 等价值**本地估算上限与原有保守 token 预约。两次配对上限合计为四槽，不等于 Plus 实际账单或用户账户 quota。任一槽非可比、usage unknown、Core denial 或限额停止时，runner 按原规则停止当前配对；下一配对只在前一配对两槽可比且授权仍足够覆盖完整窗口时才启动。TB43 的旧 unknown 请求不会重放。

新候选 [grants_tb46_tb47_pair_candidate.json](../../../scripts/terminal_bench_pilot/grants_tb46_tb47_pair_candidate.json) 与 TB45 获批文件的 10 条风险、路径、次数和内容上限完全相同，仅将每条到期时间延至 **2026-09-30 02:00（Asia/Shanghai）**；SHA-256 为 `0644d7b8628fa2478dce072d656ce726f5ba97b882cbff926d3a22b840584813`。详见两组[完整静态预检身份](TB46_TB47_STATIC_PREFLIGHT_20260929.json)。静态预检使用 2026-09-29 价格快照；若启动跨日，须先重核价并生成新的协议身份。此文件在方案形成时待批准，用户随后批准并用于 TB46；TB46 的停止条件已触发，不能依此自动启动 TB47。

本轮仍是一题、一次固定先后顺序的**探索性单变量窗口**。若结果提示质量改善，再单独申请交替顺序与多题重复；以官方任务质量为主，另报 Context 选中/实送、工具行为、token 和时长，不用一次结果声称因果降本。
