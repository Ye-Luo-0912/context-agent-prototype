# COST-6 实施回执：cache read/miss/write 归一化保留真实含义（R2-10）

日期：2026-09-13。基线 `685b6bbb` 加共享未提交树；C 线首片，按 `TASK_C_COST_CACHE.md` 交付。

## 用户结果

供应商用量按真实语义呈现：DeepSeek 的 `prompt_cache_miss_tokens`（未命中缓存的输入）不再被冒充为 cache write；显式报告的 cache-write 计数在 Chat 与 Responses 两条 transport 上都进入产品用量（此前只有 opt-in diagnostics 看得到）。未报告的计数保持 `None`，绝不折算成零；旧事件字节不被静默改写。

## 修复内容（R2-10 两个断点）

1. **Chat SSE miss≠write（`provider-openai/src/sse.rs`）**：`StreamAccumulator` 的 usage 映射改为——cache read 仍优先 `prompt_tokens_details.cached_tokens`、回退顶层 `prompt_cache_hit_tokens`；**cache write 只来自显式 write 字段** `prompt_tokens_details.cache_write_tokens`（`WirePromptTokensDetails` 新增）；顶层 `prompt_cache_miss_tokens` 进新字段 `cache_miss_input_tokens`，永不映射为 write。非法/溢出计数（字符串、超 u64）在 serde 反序列化即类型化 `MalformedEvent` 失败，不截断不回绕；不一致计数（hit+miss≠input）**原样保留、不做客户端修复**——分桶是 provider 自己的观测，客户端不重推导。
2. **Responses transport 读取 write（`provider-openai/src/responses.rs`）**：`capture_usage` 校验并读取 `input_tokens_details.cache_write_tokens`（显式 write）与顶层 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`（read 回退/miss），与 Chat 归一化逐字段一致；新计数器形状非法同样类型化失败。
3. **契约（`agent-contracts/src/model.rs`）**：`ModelUsage` 增 `cache_miss_input_tokens: Option<u64>`（serde default＋skip_serializing_if，旧 JSON 解码为 None 而非零）；`cache_write_input_tokens` 的文档更正（删除「DeepSeek miss 即 write」的错误声明）；结构体文档明确分桶包含关系——`input_tokens` 是 provider 总量、cache 桶通常是其划分/子集，关系按 endpoint/protocol 定义，**任何计数都不由其他计数算术推导，input 与任何 cache 桶相加即重复计费**。COST-2 时代已把 miss 写进 `cache_write_input_tokens` 的历史行原样保留，文档注明其不得被静默重读为可信 write 费用。
4. **跨语言消费面**：新共享金样 `event_model_used_cache_fields.json`（write=10/miss=20，含 `role:"main"`）双侧 roundtrip；Rust `work_fixtures.rs` 解码断言＋旧金样不含新字节的负断言；.NET `ModelUsageFact` 增 `CacheWriteInputTokens`/`CacheMissInputTokens`（可空，缺失即 null 绝非零）与 `Role`（默认 "main"），`TryGetModelUsage` 从类型化 `usage` 元素读取。

## 回归（红-first）

`sse.rs`：既有 `deepseek_top_level_cache_hit_and_miss_are_mapped` 断言改写为正确语义（miss→miss、write=None；旧代码上映射行即反例本体，改动前必红）＋任务书 fixture 全套——100/80/20 无 write→write=None；显式 write 字段为唯一 write 来源；部分计数缺席保持 None（两组）；不一致计数原样保留；非法/溢出计数类型化失败。`responses.rs`：显式 write 进入产品用量、顶层 hit/miss 归一化且 miss 永不变 write、部分计数/非法形状。contracts：旧 usage JSON 解码 miss=None＋None 不上 wire。protocol＋.NET：新金样 roundtrip＋访问器（.NET 侧含 legacy fixture write/miss 为 null 断言）。

## 验证（实际命令与结果）

- `cargo test -p provider-openai`：**129/129**（基线 120＋本片 8－含既有测试改写；后＋retry 标签测试）
- `cargo test -p agent-contracts --lib`：**174/174**（基线 173＋1）
- `cargo test -p agent-platform-protocol`：**47＋14/14**（含新金样测试）
- `dotnet test`（Agent.Client.Tests 全量）：**120/120**（含新 fixture 测试；计数含并行线同期新增）
- `cargo fmt --check`（contracts/provider/protocol/baselines/context-simple/eval/compose/host/tui/runtime）：通过；`cargo clippy`（本片自有 crate：contracts/provider/protocol/baselines）0 警告。
- 共享树机械补齐（如实记录）：agent-eval 两处 `ModelUsage` 字面量与 `anchor_root_misses` 构造点（A 线 CTX-5 在飞契约字段的消费点适配）、runtime actor harness 字面量——均为并行线中间态的编译适配，非本片语义。

## 未验收（如实记录）

- 未提交、未推送、未跑远端 CI。
- 真实端点的 write/miss 字段观测照旧 NOT_RUN（属 COST-5 联合验收）；「有缓存读不代表已经得出货币节省」——本片只修字段语义。
- 旧事件中已写入的 miss-as-write 数值不做重写（不可静默重新解释）；消费端何时信任历史 write 值归 COST-5 的逐端点支持矩阵确认。
