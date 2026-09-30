# TB37 Pinaic GPT-6 Luna 接入探针（2026-09-27）

## 结论

本窗口完成本地协议实现和三次各限一条请求的 Responses 工具调用探针，**没有启动 Harbor、没有形成 Dynamic/Rolling 评测或官方成绩**。三次请求访问 `https://api.pinaic.com/v1/responses` 均由上游返回 HTTP 403；relay 均未收到 usage，并各自冻结为 `budget_unknown=true`。前两次使用此前的 key；用户提供新 key 后的第三次仍为 403。新 key 的只读 `GET /v1/models` 也返回 403，安全解析出 JSON `error_type=permission_error`。Pinaic 在授权/权限层拒绝当前 key；错误正文和更细的拒绝策略未留存，无法区分 key scope、账户方案或来源策略。三次 POST 403 是否收费均未知。

首次使用旧 key 的只读 `GET /v1/models` 曾返回 200，列出 16 个模型并包含精确 id `gpt-6-luna`，响应没有价格字段；之后旧 key 和新 key 的只读 GET 都返回 HTTP 403。密钥不写入回执或仓库。Responses 探针的 403 是否计费仍未知。

## 实现与验证

- Runner 增加 `pinaic-gpt-6-luna` profile：模型 `gpt-6-luna`、Responses API、`reasoning.effort=max`、每请求输出上限 16,384、较短上下文窗口和保守请求间隔。用户随后补充 Pinaic 价格：输入 $0.40/M、缓存读取 $0.04/M、输出 $2.00/M；代码按用户提供记录，尚未与账户账单页独立核验。
- Host credential relay 支持 Responses 路径、工具名别名映射、流式函数调用名还原和 `response.usage` 结算；不改变 Core 的权限与 effect 身份决策。
- Relay 现在按 `input_tokens_details.cached_tokens` 等 usage 字段，将缓存读取以 $0.04/M 计价；没有缓存 usage 时按全价输入估算。每份回执同时记录缓存感知估算成本和全缓存未命中峰值估算，后者用于 admission stop gate。
- 新增单请求真实探针，输出只含 HTTP 状态、事件/usage 是否存在、token 数和中转冻结状态；不保存错误正文、提示词、工具参数或凭据。三次输出均为 `HTTP_ERROR`、relay HTTP 502、上游 HTTP 403、`relay_budget_unknown=true`、输出预约上限 1,024 token；新 key 探针使用用户提供的费率和 $0.02 peak-miss admission estimate 上限。
- 定向本地回归：`python -m unittest scripts.tests.test_terminal_bench_relay scripts.tests.test_terminal_bench_pilot`，32 项运行、31 通过、1 项平台专属跳过。新增合成 Responses 用量用例验证 1,000 输入（其中 800 缓存）、200 输出按用户费率结算估算 $0.000512，并同时记录全未命中估算 $0.000800；profile 断言核对 reasoning、三档费率和 $7.28 token-envelope 峰值。新只读模型探针只返回受限 `error_code`/`error_type`，不输出 provider message。
- WSL 可用；Docker 为 29.1.3，Harbor 0.23.0 位于 `/home/ye_luo/.venvs/harbor-0.23.0/bin/harbor`。没有启动 Harbor job；之前按授权清理的 `tasks-tb34` 临时题包缓存当前不存在。
- 用新 key 对 runner、测试和 TB37/CURRENT/NEXT 记录做凭据扫描：63 个文件，`account_credential_present=false`。

## 403 边界检查

不带凭据的 `GET /`、`GET /docs`、`GET /openapi.json` 均返回 HTTP 403（Server: nginx）；不带凭据的 `OPTIONS /v1/responses` 和 `OPTIONS /v1/models` 均返回 204。由此只可确认 HTTPS host 与预检路径可达；没有公开 API 文档或可供判断鉴权的响应头。使用新 key 的只读 `GET /v1/models` 返回 HTTP 403，内容类型为 JSON；安全解析器只输出 `error_type=permission_error`，不输出 message，也没有安全可输出的 `error_code`。这确认 API 权限层拒绝；具体是 key scope、账户方案或来源策略仍未知。OPTIONS 204 不证明 POST 权限。

OpenAI 官方 GPT-6 Luna 文档确认模型 id 为 `gpt-6-luna`、`reasoning.effort=max` 有效，并建议使用 Responses 完成函数调用。按用户提供的代理单价，16M 输入和 440K 输出的 token envelope 若全未命中，估算为 $7.28；当前 trial admission 的 $4 peak-miss 估算 gate 会更早停止，但它不是供应商账单硬上限。

## 继续条件

先由 Pinaic 账户侧检查新 key、账户状态、来源/IP 限制，以及模型 API 访问权限，并核实三次 403 是否收费。用户已补充代理价格；若价格与账户展示不同，应以账单页为准。确认访问恢复后再创建全新 bounded evaluation identity；在此之前不再发模型请求。若该中转站只开放 Chat Completions，则与本任务要求的 GPT-6 Luna `max` 函数调用模式不兼容，不自动降低 reasoning 或改变模型。
