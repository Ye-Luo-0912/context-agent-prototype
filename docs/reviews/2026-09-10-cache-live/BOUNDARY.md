# 最终请求复用边界与显式供应商映射

后续状态（2026-09-10）：[能力定位](CAPABILITY.md)已取得精确拒绝——服务端报告当前模型不支持 `prompt_cache_breakpoint`。下文保留本切片当时的实现和实测回执。

2026-09-10，基线 `732cf93103cb7104f1961402f422e82143b8956c` 加既有布局/观测工作树。本轮接续 [隔离复测](ISOLATED_REPORT.md) 指定的下一切片，没有重新开启 N/W 全队列或冻结实验。

**实现和本地验证完成；当前网关的显式断点兼容性未通过，迭代缓存收益尚未验收。**用户现在可以在支持的 Responses 端点上显式配置复用稳定证据的边界；默认端点继续发送原有完整请求。当前配置的实际端点拒绝了本次断点请求，因此没有启用为本机默认，也没有宣称降低费用。

## 功能与边界

- `ModelInput::into_request` 在 Runtime 最终 packing、必需正文覆盖及工具裁剪完成后绑定 `PromptReuseBoundary`。CurrentStateLast 的边界位于实际保留的 Context 消息末尾；Legacy 的保守边界位于当前状态之前的 policy 末尾。当前 turn、焦点/目录/进度不进入该复用段。
- 提示放在既有 `ModelRequest.metadata` 中，固定一个边界，包含版本、消息数量和摘要。摘要绑定有序消息的完整内容/角色及工具定义，以流式 SHA-256 计算，不复制整份 JSON、不保存上一请求。它不代表供应商 token、缓存驻留或命中。
- Provider 消费前按实际请求复验。越界、版本不认识、正文/角色/顺序/schema 改动使旧提示失效；缺失或失效时发送完整普通请求，不删除正文、不改变覆盖 ACK。最终 packing 的重新绑定覆盖旧提示。
- `OPENAI_PROMPT_CACHE_MODE` 默认 `provider_default`；只有显式 `responses_explicit` 映射一个 `input_text` 内容块的 `prompt_cache_breakpoint: {"mode":"explicit"}`，并添加顶层 `prompt_cache_options: {"mode":"explicit"}`。要求 `OPENAI_API_PROTOCOL=responses`；`auto`/`chat` 组合在创建 Runtime 之前报错。Rust 入口为 `OpenAiProvider::with_prompt_cache_mode`。
- 能力由配置声明，不从 URL 或模型别名推断；端点拒绝时不会删除参数做隐式兼容重发。普通生产重试策略仍按既有错误分类执行，实测探针没有自动重试包装器。未设置 TTL、缓存路由 key 或服务端会话；`store=false` 保留。
- 显式模式加入 provider profile digest；默认模式保留历史 digest。当前焦点、完整指令、正文、角色、工具协议配对、选择/评分/GC、Core 审批与提交语义不变。消息内容不变不能证明真实任务行为完全等价。

字段形状根据 [OpenAI 官方 Prompt caching 文档](https://developers.openai.com/api/docs/guides/prompt-caching) 核对。官方支持不等于第三方兼容端点支持；本次没有将网关别名当成上游认证。

## 实际检查

| 命令 | 结果 |
|---|---|
| `cargo test -p agent-contracts -p provider-openai --lib --quiet` | contracts 169 passed；provider 114 passed |
| `cargo test -p agent-compose --lib --test kv_cache_walk` | compose lib 31 passed；探针随后增加安全错误分类测试，最终普通运行 4 passed / 3 ignored |
| `cargo test -p agent-compose --test kv_cache_walk --quiet` | 4 passed / 3 ignored |
| `cargo test -p agent-runtime --test turn prompt_layout` | 1 passed；两种布局及两次焦点/完整指令更新 |
| `cargo test -p agent-runtime --test actor final_guard` | 3 passed；实际正文/schema 裁剪后的提示有效，超预算仍拒绝 |
| `cargo clippy -p agent-contracts -p provider-openai -p agent-runtime -p agent-compose --all-targets -- -D warnings` | exit 0；补充探针诊断后定向 `cargo clippy -p agent-compose --test kv_cache_walk -- -D warnings` 也为 0 |

回环测试验证实际 HTTP 请求保留断点与最新状态；空消息在 Responses 被省略时，映射仍使用正确的契约消息位置。默认模式与无提示请求的 wire 相同。旧请求无需新增字段即可解码。Windows 回环包测试仅在测试进程设置 `NO_PROXY=127.0.0.1,localhost`，避开已知系统代理问题。

`cargo fmt --all -- --check`、`python scripts/doc_consistency.py`（13 live docs）和 `git diff --check` 均通过。没有运行全仓测试、新 CI 或真实仓库任务，也没有创建提交、release/tag。

## 有界真实验证：2 次发送，停止于冷请求

继续沿用上一步已授权的本地 `eval.env`，不写回环境文件。只发送合成库存/焦点材料，没有仓库工具执行或私人仓库正文。每次输出上限 512、超时 55 秒、响应上限 1 MiB；显式协议且无自动重试。本轮先安排最多 4 次，首个失败后补充受限错误诊断，第二次运行上限降为 3，实际合计只发了 **2 次**。

| 记录 | 实际发送 | 结果 |
|---|---:|---|
| [首次边界探针](live-boundary-1789006879151806100.json) | 1 | 冷请求 transport 失败，1,872 ms；当时只保留错误类别，没有 HTTP 状态，不能补猜 |
| [补充错误诊断](live-boundary-1789007030166917400.json) | 1 | 冷请求 HTTP 400，`retryable=false`；固定标签匹配 `prompt_cache_breakpoint`，2,603 ms |

两次都没有模型响应诊断、回答或 usage；没有运行到原样重放或状态变化阶段。未观测到用量不表示零收费。第二次证据说明**端点拒绝当前显式断点请求**，不证明其全部缓存能力不存在，也不确定拒绝发生在网关还是上游。原始错误正文未保存，固定标签不提供更细的服务原因。

两个运行的 14 个相关源码文件分别保留在 [首次源码摘要](live-boundary-1789006879151806100/manifest.json) 与 [诊断源码摘要](live-boundary-1789007030166917400/manifest.json) 指向的本地副本；运行后均核验未变。此前 10 次与 12 次实验及其源码副本保留，未覆盖。

## 下一步

先核实此端点接受的缓存边界协议，或明确选择已支持该字段的端点/模型，再在同一边界与合成用例上验证“改当前状态仍读取稳定证据缓存”。当前环境保持 `provider_default`。不重复发送同样被拒绝的断点请求，不为缓存改焦点角色或冻结证据，不把本地回环成功写成供应商收益。
