# 本次验证与复现

起点 `8a0dc29032a3175b203545addca7549efe90a495`。工作期间共享仓库推进到 `6ec044aa869fd7ae3a6adc0ec694a4bcd42e0478`，收录了本次 W02 修改和早先生成的探针；本审查没有执行 commit/push。最新源码摘要见 [source-manifest.json](evidence/source-manifest.json)。不修改旧审查包或冻结证据。

## 已执行

| 命令/检查 | 实际结果 | 证据 |
|---|---|---|
| `cargo test -p agent-runtime --lib final_pack_coverage_requires_candidate_identity_and_complete_body`（修复前） | exit 101；different path 场景 required_misses=0，期望 1 | [红测试](evidence/final-pack-before.txt) |
| `cargo test -p agent-runtime --lib final_pack`（修复后） | 5 passed | [绿测试](evidence/final-pack-after.txt) |
| `cargo test -p agent-runtime --lib actor::model::failure_class_tests`（补充正向范围覆盖断言后） | 9 passed，包含最终缺失撤销 settlement | [相关单测](evidence/runtime-final-packing.txt) |
| `cargo test -p agent-runtime --test turn` | 120 passed，含完整指令、BeforeModel 门控取消、完成/恢复/提交边界 | [回合回归](evidence/runtime-turn.txt) |
| `cargo test -p context-simple --lib completion_boundary_storage_gc_honors_retained_checkpoint_roots` | 1 passed | [GC 恢复根](evidence/gc-retained-roots.txt) |
| 离线 `review-cache-locality` 二进制 | 9 组布局测量，结构性断言通过；只测合成消息的字节前缀 | [JSON](evidence/prefix-locality.json) |
| 离线 `residuals` 二进制（在最新 HEAD 重跑） | 3 个剩余反例成立；释放门控、停止并 join Actor 后退出 | [JSON](evidence/residuals.json) |
| `cargo clippy -p agent-runtime --lib -- -D warnings` | exit 0 | [日志](evidence/clippy.txt) |

格式、文档及差异检查的最终结果见 [checks.txt](evidence/checks.txt)。第一次调用 `cargo test ... -- --exact` 没有完整测试路径，匹配到 0 项；不计入验证，随后以正确过滤方式获得上述红测试。探针格式化不改变其测试对象。

## 复现命令

在仓库根目录运行（使用锁定的离线依赖，无 API key）：

```powershell
cargo run --manifest-path docs/reviews/2026-09-10-cache-design-8a0dc29/evidence/cache-probe/Cargo.toml --target-dir target --offline --locked --quiet --bin review-cache-locality
cargo run --manifest-path docs/reviews/2026-09-10-cache-design-8a0dc29/evidence/cache-probe/Cargo.toml --target-dir target --offline --locked --quiet --bin residuals
```

`residuals` 的断言锁定审查时的反例：未来修复后它应失败，不能把这个探针当作长期期望错误行为的产品测试。修复时应把对应期望反转并加入正式回归。它在临时目录写工件，不修改真实任务工作区；所有模型响应均为本地 mock，无远程请求。

工件测量除 `model_content_bytes` 外，另剥离行号前缀并排除换行，记录 `unframed_payload_bytes_lower_bound`；这个下界仍超过 2MiB，所以不是单纯的行号渲染开销。101 行输出合并为 100 行的问题也可由元数据独立核对。

## 证据范围

- W02 有修复前失败、修复后通过的回归；生产修改限定在 `agent-runtime/src/actor/model.rs`。
- W04 取消使用可替换 ContextEngine 的确定性门控，证明 Actor 的 AfterModel 等待不可响应；未用真实 provider 延迟作测量。Rolling 的 trigger 无差别执行模型压缩来自源码核对。
- W04 延期与 W06 长行均调用当前生产实现。HEAD 后补的 partial collapsed 守卫修复不改变 deferred_folds 反例；最新 residuals 重新验证了这一点。
- 前缀数据来自真实 PromptAssembler 与公开契约编码，未使用 provider 的隐藏渲染/分词。tools 是否相同另行报告。后置 focus 对照只改变布局，未证明行为等价或接口兼容。
- 缓存费用、cache write tokens、首 token 时延、真实任务成功率：**NOT_RUN**。本次未启用新缓存、未接只读 memo、未扩大正文缓存、未改工具执行权限。
- 没有重跑全仓测试/CI/冻结实验；旧文档中新补的 CI 记录不是本次查询验证的结果。没有检测或读取 API 凭据，不根据文档中的无凭据说明推断当前真实配置。

官方语义核对：2026-09-10 搜索后实际读取 [OpenAI Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)，用于区分前缀匹配、路由 key、模型相关断点/写入计费能力。兼容服务商的能力仍需独立声明和验证。

用户明确供应商 KV 及共同底层优先后，追加 [KV_CACHE_PLAN.md](KV_CACHE_PLAN.md)。同时通过官方搜索结果读取 [DeepSeek Context Caching](https://api-docs.deepseek.com/guides/kv_cache/)（首次直接打开超时，未据此臆测内容），核对通用前缀规律和 usage 字段差异。`sse.rs` 现有解析缺少该 usage 变体为静态发现，未在本次修改 provider 代码。共同底层不以选择任何一家供应商为前置；专属能力只作为可选适配。
