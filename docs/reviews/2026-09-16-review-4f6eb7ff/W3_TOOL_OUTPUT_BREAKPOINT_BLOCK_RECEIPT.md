# W3 回执——工具结果缓存断点块类型（V6）

- 状态：实现完成，待主会话验收提交（本回执撰写时未 commit，工作树基于 `b6ffc514`＋并行未提交改动）。
- 执行者：W3 工单（NEXT_TASKS.md 第四批），范围仅 `crates/provider-openai/src/lib.rs` 与 `crates/provider-openai/src/prompt_cache/endpoint_shape_tests.rs`。
- 并行冲突声明：会话期间另一 Agent 正在改 `provider-openai/src/retry.rs`、`agent-runtime/tests/turn/stream.rs`、`agent-contracts/src/runtime.rs` 并新增 `agent-compose/tests/cancel_usage_settlement.rs`（W4/V7）。本切片未触碰上述任何文件；`cache_routing_wire_acceptance.rs` 在动手前核对过 `git diff`——无人改动，且本切片未改它（见下）。

## 现场描述（HEAD `b6ffc514` 上）

`build_responses_wire_request` 的 declared-breakpoint 附着块（S2b `ea9c0e5c` 引入）存在三项问题：

- (a) `function_call_output` 的字符串 `output` 被包成 `{"type":"output_text", ...}` 块——`output_text` 是助手输出块类型，不是官方缓存断点的受支持宿主；
- (b) 三处「无法承载时回退 input-item sibling 字段」残留：tool 分支的 `other`、content 数组无 `input_text` 时的 fallback、content 既非字符串也非数组时的 `other`——官方形状没有 item 级断点字段；
- (c) `endpoint_shape_tests` 的工具用例函数名/注释写着 "keeps the sibling placement"，断言却已是「无 sibling、断点在块上」——名实相反，且未断言块类型（这就是 (a) 能过测试的原因）。

核实：经公共 `ModelRequest`（`ModelMessage.content: String`）这三条 sibling 分支实际不可达——builder 只会产出字符串 content/output 或 legacy 边界重写的单 `input_text` 块数组；(b) 是未经端点确认的形状残留＋「最后一项是 `function_call`（assistant 展开 tool_calls）时静默丢弃、无记录」。

## 官方文档查证（决策依据）

来源：`https://developers.openai.com/api/docs/guides/prompt-caching`（本切片重新在线核对，非仅引用审查报告）：

- 显式模式要求把 `prompt_cache_breakpoint: {"mode":"explicit"}` 加在输入消息内**受支持的内容块**上；
- 其 multi-turn agent 示例明确给出 `function_call_output` 的承载形状：`"output": [{"type": "input_text", "text": "Tool result...", "prompt_cache_breakpoint": {...}}]`——即 `output` 数组接受 `input_text` 块，工具结果是下一次请求的**输入**；
- 顶层 `instructions` 与 `additional_tools` 输入项均明确不能承载断点；全文没有 item 级（sibling）断点字段。

决策：工具结果包装 `output_text` → `input_text`（文档有直接支持，不是剔除 hint）；sibling fallback 全部移除，改为显式剔除＋记录。

## 修复机制：一处类型化映射

`lib.rs` 新增单一 seam（`pub(crate)`，测试可直接断言）：

- `BREAKPOINT_BLOCK_TYPES = ["input_text", "input_image", "input_file"]`——唯一受支持宿主类型清单；
- `DeclaredBreakpointPlacement { PlacedOnContentBlock, DroppedNoSupportedBlock }`——返回元数据即「可测试的记录」；
- `place_declared_breakpoint(&mut Value) -> DeclaredBreakpointPlacement`——唯一附着规则（见下表）。

builder 侧：`function_call_output` 取 `output`、其余 item 取 `content`，交给 seam；未落到 `PlacedOnContentBlock` 时以 `tracing::debug!(reason = "declared_breakpoint_dropped_no_supported_content_block", message_index, ...)` 记录（沿用本函数既有的 `reason` 字段日志形状，如 `explicit_only_write_policy_without_valid_reuse_boundary`）。原三条 sibling 赋值语句全部删除。

### 最终映射表（载荷 → 位置规则 → 无法承载时的行为）

| wire item / 载荷形状 | 位置规则 | 无法承载时 |
|---|---|---|
| 角色消息，`content` 为字符串 | 重写为单个 `input_text` 块数组，断点在块上（S2b 行为不变） | —（字符串总能承载） |
| 角色消息，`content` 为块数组 | 断点加在**最后一个**受支持类型块上 | 数组内无受支持块 → 剔除 hint＋返回 `DroppedNoSupportedBlock`＋日志记录，载荷原样保留 |
| `function_call_output`，`output` 为字符串 | 重写为单个 `input_text` 块数组（V6 修复：原 `output_text`），断点在块上 | —（字符串总能承载） |
| `function_call_output`，`output` 为块数组 | 断点加在最后一个受支持类型块上 | 同上剔除＋记录 |
| `function_call_output`，`output` 非 string/非 array | 不附着 | 剔除＋记录 |
| `function_call`（assistant tool_calls 展开项）及一切无 `content`/`output` 的 item | 不附着（文档不支持，也不回退到更早的同消息 item——那会缩小声明的缓存边界） | 剔除＋记录 |

legacy 单边界 hint（`boundary_index` 重写为 `input_text` 块）未重做、行为不变；Chat 方言 builder 未动（它本就不发任何缓存字段）。

## 回归（红→绿）

红-first：修复前先写测试并在 HEAD 上运行（`cargo test -p provider-openai endpoint_shape`）：

- **红**：`declared_breakpoint_on_tool_output_uses_a_supported_input_block`（原 `..._keeps_the_sibling_placement` 改名＋加强：断言 `output` 数组恰好一块、块类型 `input_text`、正文逐字保留、断点在块上、item 无 sibling）——HEAD 失败：`left: "output_text", right: "input_text"`。修复后绿。
- 诚实说明：两个无可承载块用例（`unhostable_breakpoint_is_dropped_and_expansion_does_not_shift_ownership`、`breakpoint_on_assistant_with_tool_calls_attaches_to_the_last_wire_item_only`）在 HEAD 上 wire 层已通过——HEAD 对这些形状本来就「什么都不发」，缺陷是**不可测试的静默＋死代码 fallback**，其红在于记录通道不存在（seam 及其枚举在 HEAD 上无法编译）。它们作为回归钉固定：展开后 wire 索引、断点归属（last wire item、不滑回同消息更早 item）、后续消息断点不串位、全 body 递归扫描「断点只出现在受支持块上」。
- seam 级：`place_declared_breakpoint_maps_every_payload_shape_once`——字符串→单块重写；混合数组→最后受支持块（`input_image` 也命中）；无受支持块数组/数字→`DroppedNoSupportedBlock` 且载荷不变；清单恰为三类型。
- 测试文件头（divergence-pinning 陈述）改写为当前事实：映射经单一 seam、剔除有记录、真实端点核验仍归 T8。

## 实际命令与结果（本机，Windows Git Bash）

| 命令 | 结果 |
|---|---|
| `cargo test -p provider-openai endpoint_shape`（修复前，HEAD） | 1 红（`output_text`≠`input_text`）——缺陷证明 |
| `cargo test -p provider-openai endpoint_shape`（修复后） | 7/7 绿 |
| `cargo test -p provider-openai` | **155/155 绿**（期间一度 154/155：唯一失败是并行 W4 Agent 在 `retry.rs` 新增的 `a_backoff_cancel_carries_the_known_usage_on_the_error`，非本切片文件；该 Agent 随后修复，最终全绿。窗口内以 `-- --skip retry::` 复核本切片：110 绿/45 跳过） |
| `cargo test -p agent-compose --test cache_routing_wire_acceptance` | 3/3 绿，**未改该文件任何断言**（生产 B0/B1 落在 system/user 消息上，从不落在工具结果上，B0 ContentPart 断言形状不受影响） |
| `cargo test -p agent-compose --test cache_wire_flow` | 1/1 绿 |
| `cargo clippy -p provider-openai --all-targets` | 0 警告 0 错误 |
| `cargo fmt -p provider-openai -- --check` | clean（核对过并行 Agent 的 `retry.rs` 在我运行 `cargo fmt` 前已是 fmt-clean，未触碰其格式） |

## 未做 / 取舍 / 限制

- **真实端点接受/命中/净费用**：未做，仍归 T8 条件实验；本回执只声称 wire 形状符合官方文档，不声称实测 200 或缓存收益。
- **生成侧块类型**：builder 仍只**生成** `input_text` 块（消息 content 是纯字符串）；`input_image`/`input_file` 仅在放置 seam 中作为受支持宿主被识别（为既有数组载荷与未来扩展保位），不新增生成路径。
- **网关方言**：未新增任何显式命名；Chat 方言维持字节不变（无缓存字段），Responses 显式形状即官方形状。
- **未拒绝整个缓存计划**：按工单二选一取「剔除 hint＋记录」而非拒绝整个请求——保留真实消息与工具配对，与 legacy 边界验证失败时的既有语义一致。
- 未改 `cache_routing_wire_acceptance.rs`（B0 断言无需机械适配）；未 commit。
