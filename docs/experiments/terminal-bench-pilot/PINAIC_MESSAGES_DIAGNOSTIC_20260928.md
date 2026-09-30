# Pinaic Anthropic Messages 协议诊断（2026-09-28）

## 结论

用户怀疑 Pinaic 可能只在 Anthropic API 格式下可用。本次用新 key 对 Anthropic Messages 端点做了两次各一条请求的工具调用探针：

- 标准 Anthropic `x-api-key` 认证：`POST /v1/messages`，`anthropic-version: 2023-06-01`，模型 `gpt-6-luna`，`max_tokens=128`，强制一个极小的 function tool。
- 同一 Messages 请求改为 `Authorization: Bearer` 认证，再验证 Anthropic 文档允许的另一种 API key 头格式。

两次均返回 HTTP 403，JSON `error_type=permission_error`；没有返回 usage。探针只输出状态、错误类型与 token 元数据，不打印 provider message、回答文本、工具参数或凭据。两次请求是否计费未知。

结合同一新 key 的只读 `GET /v1/models` 也返回 `permission_error`，以及 TB37 中 OpenAI Responses 多次 403，可以确认目前问题不是仅由 OpenAI Responses 线缆格式造成。Pinaic 权限层拒绝该 key 或账户来源；返回信息没有给出具体 key scope、账户方案或来源策略错误码。未启动 Harbor 或 Terminal-Bench 正式评测；Anthropic 协议探针只验证 Pinaic 端点，不代表仓库运行时已支持 Anthropic provider。

## 安全与继续条件

API key 仅在内存中经静默 stdin 发出，不保存在回执。Pinaic 账户侧需检查 key 权限、账户状态、模型权限及 IP/来源策略，并核实两次 403 是否收费。访问恢复后再建全新评测窗口；旧 probe 不自动重放。
