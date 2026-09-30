# TB48/TB49 Dynamic 限定复测方案（2026-09-29）

**执行结果：** 用户批准后 TB48 在第 19 次上游请求遇 HTTP 503 和 usage unknown，Dynamic 非可比，TB49 依停止规则未启动。以下为开窗前原定方案；实际事实见 [TB48 中止回执](TB48_ABORTED_20260929.md)。

TB46 Dynamic 在第 56 次上游请求遇到用量 unknown 后，按用户批准的“任一非可比即停止”条件结束，Rolling 与 TB47 均未启动。原始收据和不能归因的 `ValueError` 见 [TB46 中止回执](TB46_ABORTED_20260929.md)。之后 relay 只增加了本地流错误的固定类别与已读字节数，继续保留未知预约并冻结；畸形 SSE 的 localhost 负例和 Python 全套回归通过。TB46 第 56 次不会重放或补账。

下一次仅建议两个**独立 Dynamic 槽**，共最多 112 次模型决策；复测授权不沿用先前已触发的停止许可。TB48 使用旧基线 ELF，先观察完整 56 轮能否结算；只有 TB48 Runtime completed、Harbor 有正式评分、无 Core deny 与 unknown 时，才启动 TB49 新 ELF。两槽在相同的题包、镜像、EasyCLI `gpt-6-luna` Responses/max、host relay、Core grant 和预算下运行，生产二进制只差 Context headroom 额度修正。

| 顺序 | 新 identity | ELF SHA-256 | 零供应商预检 |
| --- | --- | --- | --- |
| 1 | `TB48_BUDGET_BASELINE_DYNAMIC_56_20260929` | `88bbf059d1ec56b9cf7abf097a6ab22222ddecde4635212f74e75e1c3bd5fb4d` | `STATIC_READY_NO_PROVIDER_CALLS`，目标 Debian 12 镜像 ABI PASS |
| 2 | `TB49_BUDGET_TREATMENT_DYNAMIC_56_20260929` | `119ba505c31f2444d6674980b62af0d052041ab57e4009ceceff97db568bb262` | `STATIC_READY_NO_PROVIDER_CALLS`，目标 Debian 12 镜像 ABI PASS |

每槽上限仍为 56 决策、70 次上游尝试、8M input、440k output、1400 工具尝试、8400 秒以及原有 relay 峰值价格等价物估算限制；账户侧沿用用户指定的 Plus 会员 quota 上限，relay 无法报告实际账单。两槽上限合计为 16M input、880k output、140 次上游尝试。使用同一份先前已批准、到 **2026-09-30 02:00（Asia/Shanghai）** 的 [10 条 Core grant 文件](../../../scripts/terminal_bench_pilot/grants_tb46_tb47_pair_candidate.json)，SHA-256 `0644d7b8628fa2478dce072d656ce726f5ba97b882cbff926d3a22b840584813`；本方案没有修改 grant 内容或有效期。两份[完整静态预检](TB48_TB49_STATIC_PREFLIGHT_20260929.json)的 relay 摘要相同，为 `0a8ef200c334a5cfa408b6833eafeb3d62751bd5fda68e79ebcdd790ea7f0775`。

新 relay 收据可把上游事件 JSON 格式错误、SSE 单行超界、总流超界等**本地**失败分开记录，同时仅保存字节计数与固定类别，不保存错误正文。若同样出现用量 unknown，保持预约并立刻停止，下一槽不启动。若两个槽都可比，单题单次固定顺序只能给出探索性结果；先核对官方任务质量、失败阶段、Context 实际选中与输入可见性，再决定是否值得申请换序/多题重复。

本方案形成时两份静态预检在 2026-09-29 当日价格快照下通过，模型请求为 0。若运行跨入下一自然日，runner 会要求重新核对价格并生成新协议身份；若剩余 grant 时间不足完整 bounded agent 窗口，开窗前会拒绝。用户随后批准越过 TB46 停点；TB48 实际结果与新的停止条件见上方执行状态。
