# KV 发送边界观测切片

2026-09-10；在共同布局第一切片之上继续，保留 Core/Runtime/Context 职责。用户现在可以区分“本地最终请求保持前缀”与“供应商实际报告缓存读取”，同时看到服务端自报模型和可选的缓存写入量。此入口用于定向诊断；没有新增数据库、后台工作器或第二套上下文状态。

## 已实现

后续补充（2026-09-10）：`OpenAiCallObserver::on_http_error` 提供非成功 HTTP 响应的状态及可选、精确识别的缓存拒绝枚举；不向观察者暴露任意错误正文，也不改变调用的成功/失败判定。该回调与 SSE 响应快照分开，既有 observer 可沿用默认实现。实际能力拒绝和新增检查见 [CAPABILITY.md](CAPABILITY.md)。

`OpenAiProvider::complete_stream_observed(request, sink, observer)` 与普通调用共用发送/解析路径。每次调用传入独立 observer；provider 不保存上一请求或上一响应，避免并发任务串号。普通 `ModelTransport` 传入 None，不计算诊断指纹。

- 请求摘要取自 reqwest 已构建的最终 HTTP body，随后原样发送该 request；包含完整 body SHA-256、字节数、按序消息/调用项指纹、工具定义和其余顶层配置指纹。
- 字节前缀摘要每 1,024 字节一个块，最多 256 块；消息项最多 128 项，超过上限明确标注不完整。相同连续块只能给出字节公共前缀下界，不能换算为供应商 token 或缓存命中。
- 响应保留 created/completed/failed/incomplete 的来源，并采集有限长度的 model/system_fingerprint。模型名只是网关自报，不能当成上游独立认证。
- 数值字段包括 input/output、cache read、cache write、cache miss。Responses/OpenAI 的 details 字段与兼容 Chat 的 hit/miss 字段分别识别；miss 不当成 write。未知为 None，零值保留，别名冲突/畸形字段明确列出，不靠猜测补齐。
- observer 不接收 HTTP headers、端点、凭据、提示正文、工具参数或回答正文。回调必须短且不 panic；由调用方决定是否保存摘要，没有默认持久化。
- 响应诊断只是服务方报告的快照。它不改变严格协议解析和成功/失败判定；调用是否成功仍以既有 `AgentResult<ModelOutput>` 为准。Auto 协议可能产生两次发送，定向实测强制显式协议以约束调用数量。

本片没有扩展 RuntimeEvent/客户端协议，也没有宣称普通宿主事件已经持久记录这些诊断。现有 `ModelUsage` 仍保持原契约；新字段在按需调用返回的 observation 中。

## 验证

本地 HTTP 回环测试分别覆盖 Chat 和 Responses：同一请求在普通/observed 路径的实际接收字节相等，记录的完整 SHA-256 与服务器收到的 body 相等；末尾 System 状态位置保留，服务自报模型与配置别名分开。其他测试覆盖观测容量、正文不进入摘要、零/未知/写入/未命中/冲突的区分。

实际执行：

```text
cargo test -p provider-openai --lib
110 passed

cargo test -p agent-compose --test kv_cache_walk
2 passed, 2 ignored（真实调用只由显式 ignored 命令开启）

cargo clippy -p provider-openai -p agent-compose --all-targets -- -D warnings
exit 0
```

真实调用与数据另见后续实测报告；本地测试证明发送一致性和诊断正确性，不证明缓存收益。

## 判断框架

共同底层的目标是最小化任务总费用，约束是当前指令、合格证据、权限和验证语义完整。若供应商将输入划为互斥的普通输入 U、缓存读取 H、缓存写入 W，则 `N = U + H + W`，费用为 `U·p + H·r + W·w + O·o`。分类与价格未知时不代入零，也不拿 `H/N` 直接充当节省比例。

公共前缀 P、服务方已经保存的前缀集合 K、可查找的边界 B、路由 R 是不同条件。只有同时匹配，P 才可能变成实际读取 H。因而之后共同契约应表达“此处以前有资格复用”，由薄适配层选择支持的缓存边界或自动缓存策略；任何不支持的环境仍发送完整请求，不能牺牲焦点角色或正文时效性来凑命中。

[OpenAI 当前缓存文档](https://developers.openai.com/api/docs/guides/prompt-caching)明确区分公共前缀、已经写入的缓存边界和后续查找边界，且不同模型的自动/显式规则不同。该说明用于约束假设，不能证明第三方网关采用同一实现或费率。
