# EasyCLIProxyAPI 本地 GPT-6 Luna 接入（2026-09-28）

## 结果

EasyCLIProxyAPI 已在 Windows 本机运行，其 API 监听 `127.0.0.1:8317`；WSL Ubuntu 对 `http://127.0.0.1:8317/v1/models` 的只读访问返回 HTTP 200。模型列表包含精确 ID `gpt-6-luna`。用户界面显示已登录的 Codex Plus 凭证，并报告成功请求；这不单独证明下面探针实际选择了哪份上游凭证。

在用户明确授权的一次、`max_output_tokens=128` 的本地生成探针中，`POST /v1/responses` 使用 `gpt-6-luna`、`reasoning.effort=max` 和一个强制函数工具，返回 HTTP 200、`status=completed`、一个带 `call_id` 的函数调用。返回 usage 为 input 349、output 19、cached input 0 tokens。探针只输出状态、模型匹配、工具调用和 usage 摘要；未保存回答正文、工具参数、OAuth 文件内容或访问密钥。用户授权的这一次生成请求已用完；本次接线与验证没有继续调用模型。

## 接线与账本口径

`run_bounded_b_pair.py` 新增 `easycli-local-gpt-6-luna` profile：Responses API、`max` 推理、`http://127.0.0.1:8317/v1`。本地 profile 不从 stdin、环境或旧 provider 配置读取账户密钥。host relay 仅对此显式模式允许无上游 Authorization，且要求 HTTP 目标精确位于 `127.0.0.1`；该请求禁用环境 HTTP proxy。Harbor 容器仍只获得独立的 trial token，模型请求数、输入/输出 token 和 Core 工具授权边界沿用原 runner。

账本使用 [OpenAI GPT-6 Luna 公开 API 价格](https://developers.openai.com/api/docs/models/gpt-6-luna)（input $0.10/M、cached input $0.01/M、output $0.50/M）计算**直接 API 价格等价估算**。这不是 EasyCLI/Codex Plus 的实际账单，也不限制订阅账号的 5 小时或周额度；`price_snapshot` 将 `subscription_quota=true`、`proxy_price_verified=false` 和独立的 `limit_type` 明确记录。既有美元上限只约束此等价估算，真实评测仍需要新的账号额度授权、独立试验身份和已验证的冻结输入。公开价格按 2026-09-28 核查；脚本在日期变化后拒绝沿用旧价快照。

## 验证与限制

- `python -m unittest scripts.tests.test_terminal_bench_pilot scripts.tests.test_terminal_bench_relay`：34 项运行，33 通过、1 项平台条件跳过。新增回归覆盖本地 profile 不读取别家密钥、拒绝 stdin 密钥、匿名上游只能是回环地址、转发时不附带 Authorization、模拟 Responses SSE usage 结算。
- `python -m py_compile` 四个受影响 Python 文件：通过。
- `python scripts/doc_consistency.py`：文档结构/链接门通过。
- `git diff --check`：通过。
- 没有启动 Harbor/Terminal-Bench 任务，也没有取得官方 grader 分数。一次非流式函数调用成功，不等于长流式任务或完整 Dynamic→Rolling 配对可用；本地代理所选上游凭证与实际订阅额度消耗未独立验证。

Pinaic `https://api.pinaic.com/v1` 的 `permission_error` 是另一条路径；本地 EasyCLI 成功不改变该 403 诊断。
