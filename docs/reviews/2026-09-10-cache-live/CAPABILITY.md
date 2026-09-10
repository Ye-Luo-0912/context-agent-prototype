# 当前模型的显式缓存拒绝：定位与诊断补齐

后续（2026-09-10）：用户选择 DeepSeek Flash 后，[共同布局的合成缓存复用已验收](DEEPSEEK_FLASH.md)，使用该服务的默认缓存。下文仍描述此前 `api.pinaic.com` 路由的显式断点拒绝，不代表 DeepSeek 的能力或测试结果。

2026-09-10，接续 [BOUNDARY.md](BOUNDARY.md)，HEAD 仍为 `732cf93` 加既有工作树。

**拒绝原因已查明：服务端明确报告当前模型不支持 `prompt_cache_breakpoint`。**沿用 `eval.env` 中的 `api.pinaic.com` / Responses / `gpt-5.6-luna`，小合成请求收到 HTTP 400、`retryable=false`，脱敏控制台回执中的错误为：

> prompt_cache_breakpoint is not supported on this model

错误类型由服务端报告为 `upstream_error`。这是当前路由的能力拒绝声明，不能认证实际上游模型身份，也不能证明该供应商的所有模型或其他缓存机制都不支持。请求形状仍与 [OpenAI 官方显式断点示例](https://developers.openai.com/api/docs/guides/prompt-caching) 一致；没有找到这家供应商可公开检索的具体缓存接口说明。此路径的显式断点收益验收受阻，不继续原样重试，也不按模型别名自动开启能力。

## 本轮实际尝试

本轮共 **3 次小合成请求尝试**，每次输出上限 128，没有自动重试。只测试字段接受情况，正文低于缓存收益测量规模。三次都没有取得模型回答或 usage；不把未观测用量写成零收费。

| 记录 | 请求 | 结果 |
|---|---|---|
| [独立 HTTP 小探针](capability-shape-1789009725395745200.json) | 普通 content-array 对照 | 20,735 ms 后 ReadTimeout，没有保存 HTTP 状态，显式分支未执行 |
| [正式 Rust 传输对照](live-capability-1789009913444282500.json) | provider_default | 55,016 ms 后发送错误；1 条发送前摘要，没有 HTTP 响应，显式分支未执行 |
| [正式 Rust 传输字段探针](live-capability-1789010115432453900.json) | responses_explicit，先验证字段 | 10,072 ms 后 HTTP 400，明确报告断点不受当前模型支持；普通对照分支未再执行 |

最后一次的精确错误来自本轮脱敏控制台回执；原 JSON 只保存状态和固定标签，未回填或伪装成已保存原始响应。此前两次超时是独立的可用性观察，不用于推断断点能力。另做了无凭据 HEAD 连通检查：端点根路径返回 HTTP 404、总计约 0.77 秒；当时本机代理 `127.0.0.1:10808` 正在监听。这只证明该次根路径请求可达，不证明模型生成服务正常。

两个 Rust 运行各保留 15 个相关源码文件副本及运行后未变校验：[普通对照源码](live-capability-1789009913444282500/manifest.json)、[字段探针源码](live-capability-1789010115432453900/manifest.json)。两个 ignored test 的进程成功表示诊断流程运行结束，**不表示 API 调用成功或缓存兼容性通过**；应读报告中的失败字段。

## 已交付的诊断能力

调用 `complete_stream_observed` 的开发者现在可以通过默认兼容的 `OpenAiCallObserver::on_http_error` 收到 `WireHttpErrorObservation`：仅协议、HTTP 状态和可选 `reported_cache_rejection`。普通未观测调用不解析这些诊断字段，也不新增存储。

- 针对 HTTP 400/422 的有界 JSON，只把精确的已知拒绝语句分类为 `BreakpointUnsupportedByModel` 或 `OptionsUnsupportedByModel`。这些枚举表示服务端报告，不是本地认证的能力事实。
- 未知、畸形、过大的响应正文或其他状态保留 `None`；不从任意正文关键词猜原因，不把参数、提示、URL、凭据或任意错误正文放进回调。
- Chat 和 Responses 的非成功响应均可观测；原来的错误返回、是否可重试、Auto 协议协商和请求字节保持原行为。诊断不会关闭显式模式、去掉字段重发或改变能力配置。
- 真实拒绝定位后才加入此类型化回调。本轮用实际错误形状的 HTTP 回环夹具验证它，没有为验证新增回调再次调用已拒绝的真实接口。后续探针会在报告的 `http_errors` 数组记录该回调。

实际检查：

```text
cargo test -p provider-openai --lib --quiet
116 passed

cargo test -p agent-compose --test kv_cache_walk --quiet
5 passed, 4 ignored

cargo clippy -p provider-openai -p agent-compose --all-targets -- -D warnings
exit 0
```

新增回归覆盖精确拒绝、未知/畸形/超界/非参数错误不误判，以及两个协议的真实 HTTP 回环：普通和 observed 请求字节相同、错误与不可重试判定相同、只生成 HTTP 错误观测、不伪造 SSE/模型结果。

当前继续使用 `provider_default` 与已落地的共同布局/复用边界；配置文件、模型和焦点/上下文策略未改。继续显式缓存收益验收需要一个已确认支持该字段的端点/模型。此供应商限制不改变现有 W 队列的归属与其他修复顺序。
