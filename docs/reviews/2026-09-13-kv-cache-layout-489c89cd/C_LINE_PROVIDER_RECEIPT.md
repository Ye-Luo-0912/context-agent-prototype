# C 线回执：provider-openai 三切片（C1 / C2 / C4）

日期：2026-09-13
实施者：C 线（provider-openai）子代理
本地工作树起点：`3d42f57`（ZCODE_TASKS.md 基线 `489c89cd` 之后；工作树内另有并行线对
`agent-process/*`、`agent-compose/tests/proof_supervision.rs` 的未提交改动，本切片未触碰）。
未 git commit / push。未触碰 agent-runtime、tool-runtime、context-simple、agent-compose 其他文件。

## 改动文件

- `crates/agent-contracts/src/model.rs`（契约：新增字段与枚举）
- `crates/provider-openai/src/lib.rs`（Responses/Chat mapper、流内取消与 Done-sink 出口）
- `crates/provider-openai/src/prompt_cache/tests.rs`（C1/C2 wire + 实际 HTTP 回归）
- `crates/provider-openai/src/retry.rs`（C4 用量账目 + 回归）
- `crates/agent-compose/src/compactor.rs`（仅 C2 的 compactor 部分：声明写策略）
- `crates/agent-contracts/src/model_cache.rs` 未改动（`PromptReuseBoundary` 完整性语义保持原样）；
  `error.rs` 未改动（非本切片所有权，`FailedWithUsage` 基建原样复用）。

## 契约新增字段清单（全部加法、旧 JSON 兼容）

| 类型 | 新增 | serde |
|---|---|---|
| `ModelRequest` | `prompt_cache_key: Option<String>` | `#[serde(default, skip_serializing_if = "Option::is_none")]` |
| `ModelRequest` | `cache_write_policy: Option<CacheWritePolicy>` | 同上 |
| `enum CacheWritePolicy` | `ExplicitOnly` | `#[serde(rename_all = "snake_case")]` → `"explicit_only"` |
| `CallStage`（provider-openai 观测类型，Serialize-only） | `known_usage: Option<ModelUsage>` | `#[serde(skip_serializing_if = "Option::is_none")]` |

未改：`ModelUsage`、`PromptReuseBoundary`、`ModelInput`、`ModelChunk`、`AgentError`。
`ModelRequest` 未设键/策略时序列化字节与历史完全一致（`agent-platform-protocol` 全绿佐证）。

---

## C1（R2）：已确认端点发送稳定 prompt_cache_key

### 改动

- `build_responses_wire_request`：仅在 `OpenAiPromptCacheMode::ResponsesExplicit`
  （配置期已验证必须 pinned Responses 协议）且 `request.prompt_cache_key` 为 Some 非空时，
  把键写入 Responses wire 顶层 `prompt_cache_key` 字段。transport 不派生、不改写键值。
- Chat mapper 不写该字段：本适配器对 Chat 方言没有已确认能力信号（explicit 模式仅
  Responses、且配置期校验），按「不确定的端点不发送专属字段」处理。
- `ProviderDefault` 模式（未确认能力）任何情况下不写键。

### wire 断言（测试）

- `confirmed_explicit_profile_sends_a_byte_stable_prompt_cache_key`：同一请求两次构建
  wire 序列化字符串逐字节相等；键出现在顶层 `prompt_cache_key`；下一轮只改尾部内容键不变；
  跨隔离域键不同、去掉键字段后两 payload 完全相等（键是唯一命名空间差异）。
- `unconfirmed_endpoints_and_keyless_requests_keep_historical_payloads`：
  ProviderDefault 模式带键不写字段；无键时 ResponsesExplicit payload 无该字段；
  Chat 带键/无键字节相等。
- `actual_http_payloads_carry_the_stable_cache_key_per_isolation_domain`（真实 HTTP）：
  loopback mock 捕获 3 个实际 payload——同任务连续两轮 `prompt_cache_key` 相同，
  第三个请求（另一隔离域）键不同。

### 红/绿证据

- 红（mapper 改动前）：`cargo test -p provider-openai --lib prompt_cache` →
  `5 passed; 3 FAILED`。行为红两项：`confirmed_explicit_profile_sends_a_byte_stable_prompt_cache_key`
  （键未上 wire）、`explicit_only_policy_without_a_valid_boundary_sends_the_no_write_shape`
  （C2 形状缺失）；`write_policy_never_changes...` 当时因测试自身一个对照断言夹具错误同时失败
  （把有合法边界的请求当成了无边界请求，已修正夹具后作为对照绿）。既有 4 个历史行为测试全过。
- 绿（mapper 改动后）：`prompt_cache` 过滤 10 passed；全 crate 147 passed。

---

## C2（R3）：显式写策略与空边界

### 改动

- 契约：`CacheWritePolicy::ExplicitOnly`（语义：本调用属于已确认 explicit-only 的端点，
  无合法边界时不得产生缓存写）。
- `build_responses_wire_request`（仅 `ResponsesExplicit` 确认模式下生效）：
  - 有合法边界 → 原形状不变（断点 + `prompt_cache_options {"mode":"explicit"}`）。
  - 无合法边界（缺 hint 或 hint 过期失效）且 `cache_write_policy == Some(ExplicitOnly)` →
    发送「零断点的 explicit 形状」：`prompt_cache_options {"mode":"explicit"}` 且全 payload
    无任何 `prompt_cache_breakpoint`——已确认「explicit 且零断点不缓存」的端点上即无缓存写；
    并以 `tracing::debug!(reason = "explicit_only_write_policy_without_valid_reuse_boundary")`
    记录原因。
  - `ProviderDefault`/未知能力端点（含 Chat 方言）：无论是否带策略，payload 逐字节不变
    （策略是调用方期望，不是能力开关）。
- `agent-compose/src/compactor.rs::compact`：构造的 `ModelRequest` 现在设置
  `cache_write_policy: Some(CacheWritePolicy::ExplicitOnly)`（维护调用标记自己的策略；
  `prompt_cache_key: None`——键由运行时/组合根决定，compactor 不发明键）。
  显式 transport 下压缩调用不再静默走「让供应商隐式缓存一次性后缀」。

### wire 断言（测试）

- `explicit_only_policy_without_a_valid_boundary_sends_the_no_write_shape`：
  缺 hint → options=explicit 且零断点；过期 hint（digest 失配）→ 同形状、全 payload 不含
  `prompt_cache_breakpoint`；有合法边界 → 断点照常写入（策略不阻止合法写）。
- `write_policy_never_changes_unconfirmed_or_default_payloads`：
  ProviderDefault 模式带/不带策略字节相等；Chat 带/不带策略字节相等；
  确认模式不带策略的历史回退行为不变（无 options 字段）。
- `actual_http_payload_honors_the_explicit_only_no_write_shape`（真实 HTTP）：
  无边界 payload 带 `prompt_cache_options=explicit` 且不含断点；有边界 payload 断点在位。
- compactor：`compaction_requests_declare_the_explicit_only_cache_write_policy`。

### 红/绿证据

- 红（mapper 前）：同上 3 个 FAILED（含 C2 的 no-write 形状缺失）。
- 红（compactor，行为级）：把 `compact` 的策略字段临时置回 `None` 运行 →
  `compaction_requests_declare_the_explicit_only_cache_write_policy` FAILED：
  "the maintenance lane must mark its own explicit-only write policy"（assertion left/right）；
  置回 `Some(ExplicitOnly)` → passed。
- 绿：`prompt_cache` 10 passed；`cargo test -p agent-compose --lib compactor` 10 passed
  （其中新增 1）。

---

## C4（R6）：每次真实 attempt 的用量账目

### 改动（全部在 `provider-openai/src/retry.rs` 与 `lib.rs`，复用 COST-7 `FailedWithUsage` 通道）

- `KnownAttemptUsage` 聚合器替代原「最近一笔」暂存：每个真实 attempt 的错误只 settle 一次
  （SSE 累计快照在 accumulator 内已收敛为该 attempt 的最终快照——`sse.rs`/`responses.rs`
  均为 replace 语义，新增 lib.rs 测试钉住），不同 attempt 的已知计数相加（饱和）；
  未报告的尝试保持缺席，绝不补零（`merge_known_usage` 只合并双方至少一方上报的计数）。
- 终态语义（live / buffered / `complete()` 三条路径一致）：
  - give-up / 非重试 / emitted 屏障失败：`terminal_error` 把聚合用量经
    `AgentError::FailedWithUsage` 随错误返回；原错误若是 `FailedWithUsage` 则展开取其
    `source`，不嵌套包装（`failure_source()` 分类保持正确）；无任何上报则保持原 plain 错误。
  - success：失败 attempt 的已知用量合并进返回的 `output.usage`（再盖 attempts/retries）。
  - cancel：**保持 plain `AgentError::Cancelled`**——`agent-runtime/src/actor/model.rs:1608`
    精确匹配该变体触发取消屏障，包装会把用户取消变成 provider 失败；已知用量改挂在
    终态 `CallStage.known_usage`（JSONL 观测通道，`OPENAI_RETRY_METRICS_FILE`）。
    同时修复一个既有潜在缺陷：mid-stream 取消（inner 返回 `FailedWithUsage{Cancelled}`）
    此前会以包装形态漏给运行时造成 Failed 误分类，现在统一解包为 plain Cancelled，
    其用量并入 stage 记录（lib.rs 取消出口现携带已收到的用量证据）。
  - sink 失败（live 的 Retrying 通知失败、buffered 的回放失败 / take 失败）：走同一
    `finish_failed`，已知用量不丢；buffered 回放失败时成功 attempt 自身的 usage 也并入。
- lib.rs：chat/responses 的流内取消出口读取 accumulator 已收到的用量后随错误带走；
  两处 Done-sink 错误出口以 `failed_with_usage(usage, error)` 保留已计费 attempt 的计数
  （空 envelope 自动退回原错误）。

### 停止条件回归（全部落在 `retry::tests` 与 lib.rs）

1. failure-with-usage → success：
   `a_usage_reported_before_success_is_kept_and_counted_once`（live+buffered：90+10 / 30+2 / cached 60，
   attempts=2，恰一次）；`a_usage_wrapped_transport_failure_stays_retryable` 追加合并断言（complete 路径）。
2. 两次 failure-with-usage → give-up：
   `two_usage_reporting_failures_sum_exactly_once_on_give_up`（90+40=130、30+5=35，恰一次；
   末次失败自带用量时合并而非覆盖；`failure_source` 保持 Transport-retryable 无嵌套）。
3. usage 后取消：
   `cancel_after_a_reported_usage_keeps_plain_cancelled_and_the_stage_record`（plain Cancelled +
   stage `known_usage={90,30}`、outcome=Cancelled）；
   `a_mid_stream_cancellation_keeps_the_plain_variant_and_the_stage_usage`（90+7=97、30+3=33）。
4. sink failure：`a_sink_failure_keeps_the_reported_usage_of_every_attempt`
   （live：重试通知失败保留 {90,30}；buffered：回放失败保留两次 attempt 合计 {100,32}）。
5. unknown 不补零：`a_give_up_without_any_reported_usage_stays_unknown`（错误无 envelope、
   stage `known_usage=None`）；同 attempt 多条快照只结算一次：
   `chat_usage_snapshots_within_one_attempt_settle_once`（60→100 两条累计快照，失败携带 100 而非 160）。

### 红/绿证据

- 行为级红检查（不改文件归属、不回滚工作树）：临时禁用合并语义
  （success 合并改为丢弃、`KnownAttemptUsage::settle` 改为 replace-only，等价旧循环
  「最近一笔 + 成功丢弃失败用量」的可观测行为）后运行：
  `cargo test -p provider-openai --lib retry::tests` →
  `39 passed; 5 FAILED`——`a_usage_reported_before_success_is_kept_and_counted_once`、
  `two_usage_reporting_failures_sum_exactly_once_on_give_up`、
  `a_mid_stream_cancellation_keeps_the_plain_variant_and_the_stage_usage`、
  `a_sink_failure_keeps_the_reported_usage_of_every_attempt`、
  `a_usage_wrapped_transport_failure_stays_retryable` 全部转红；
  恢复实现后全绿。旧 API（无 `CallStage.known_usage`）下取消路径的 stage 断言不可编译，
  其「旧行为丢用量」由 R6 审计原文与红检查的 replace-only 变体共同覆盖。

---

## 验证输出（本机实际执行）

```text
cargo test -p provider-openai                 → lib: 147 passed; 0 failed（其余 target 0）
cargo test -p provider-openai prompt_cache    → 10 passed; 0 failed
cargo test -p provider-openai retry           → 48 passed; 0 failed
cargo test -p agent-contracts                 → 176 passed; 0 failed
cargo test -p agent-compose --lib             → 39 passed; 0 failed
cargo test -p agent-platform-protocol         → 54 passed / 15 passed（金样不变，全绿）
cargo test -p agent-compose --test kv_cache_walk（未加 --ignored）→ 5 passed; 5 ignored（付费实验照旧忽略）
cargo clippy -p provider-openai -p agent-contracts -p agent-compose --all-targets → 无告警
cargo fmt -p provider-openai -p agent-contracts -p agent-compose -- --check → 通过
cargo check -p agent-runtime -p agent-eval -p context-simple -p context-baselines → Finished
  （契约为加法字段，依赖方无需改动即可编译）
```

本切片新增测试：provider-openai 13（prompt_cache 6、retry 6、lib.rs 1）、
agent-contracts 1、agent-compose/compactor 1；修改 1 个既有测试的断言
（`a_usage_wrapped_transport_failure_stays_retryable`，追加合并用量断言，原断言不变）。

## 边界与残余

- Chat 方言不发送 `prompt_cache_key`（无已确认能力信号）；将来如确认某 Chat 端点支持，
  属新能力信号，需按端点 profile 显式加入，不得按域名/模型别名猜测。
- `prompt_cache_key` 的稳定性、非敏感性与命名空间隔离是调用方契约，transport 不校验派生方式。
- 取消路径的已知用量在 JSONL stage 记录（`known_usage`）可见；运行时错误通道保持 plain
  `Cancelled`（这是有意取舍，见 C4 节）。直连 `OpenAiProvider`（不经 `RetryingTransport`）的
  调用方在「流内取消且已收到用量帧」时可能观察到 `FailedWithUsage{source: Cancelled}`——
  生产组合（agent-compose、agent-eval）都在 retry 包装之后，且须以 `failure_source()` 判类。
- 真实供应商对照实验（付费）未执行，按规则不计为验收。

---

## 给运行时侧的接口请求（请主会话转交 / 决策「谁填 key」）

1. **谁填 `prompt_cache_key`**：由运行时/组合根在请求构建点填写——
   `agent-runtime/src/actor/model.rs` 组装 `ModelInput::into_request(...)` 之后、
   `complete_stream` 之前，设置 `request.prompt_cache_key = Some(key)`。
   建议键形状：`{隔离域}|{workspace_id}|{端点/profile 标识}|{task_id}|{lane}`，
   其中 lane 区分 Main / Maintenance。约束：同一持久任务的连续请求（含冷恢复同一任务身份）
   键不变；跨隔离域不混用；禁止每轮 UUID、model_round、完整请求 hash。
   transport 只在 `OPENAI_PROMPT_CACHE_MODE=responses_explicit` 的确认 profile 上写 wire。
2. **Maintenance lane 是否共享命名空间**：compactor 请求目前 `prompt_cache_key: None`
   （它不发明键）。若运行时希望压缩调用进入同一路由命名空间，请选择一种方式注入：
   给 `ModelBackedCompactor::new` 传键工厂，或由运行时侧包装 transport 统一盖章。
   这是运行时侧决策，C 线不预设。
3. **C4 对运行时零改动即受益**：失败 give-up 的 `error.reported_usage()` 现在返回
   全部已上报 attempt 的聚合（运行时 `agent-runtime/src/actor/model.rs:1618` 已在消费，
   无需改动）；取消仍匹配 `AgentError::Cancelled`，其已知用量在 retry 观测
   stage 记录的 `known_usage` 字段（JSONL）中，不会丢失。若运行时希望把取消 attempt 的
   已知用量并入成本账本，请从 stage 记录取——不要要求错误通道携带（会破坏取消屏障匹配）。
4. **`CacheWritePolicy::ExplicitOnly` 的主调用使用**：当前只有维护 lane（compactor）声明。
   若运营确认某端点为 explicit-only 且希望主调用也声明，请运行时在主请求上设置
   `cache_write_policy = Some(CacheWritePolicy::ExplicitOnly)`；transport 仍只在
   `responses_explicit` 确认模式下生效，ProviderDefault/未知端点 payload 不变。
