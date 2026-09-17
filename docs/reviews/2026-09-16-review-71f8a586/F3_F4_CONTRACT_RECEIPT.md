# F3/F4 参数契约切片回执（第九批 · 71f8a586）

日期：2026-09-17。分支 `f34-contract`（worktree `cap-contract`），基线 `dfbea60d`，两个提交：`aa83f65e`（F3）、`d84d6ee8`（F4）。审查材料（REVIEW.md F3/F4 节、NEXT_ACTIONS.md §2、SCHEMA_CASES.json、MECHANISM_CHECKS.json）已完整读取；`schema_profile.rs`、`jcs.rs` 全文完整读取；Core 消费点（`kernel/mod.rs::execute_published_tool` 及 identity fence）、`agent-runtime/src/surface.rs::compile_schema_profiles` admission 路径、`ArgumentDigest` 持久化/比较面（`operation.rs`、`port.rs`、`broker.rs`、`kernel/operation.rs` 恢复路径）为定向读取；E2 兼容模式取自 `git show effd09aa`。审查描述与实际代码逐条核对一致后再动手。

## F3（`aa83f65e`）：Schema 编译接受约束却在节点构造中静默丢弃

**用户故障：** `{"type":"boolean","enum":[false]}` 的工具，模型提交 `{"flag":true}` 能通过 Core 在审批前的 schema 门禁并到达 dispatcher——编译器读取并检查了 `enum`（唯一性/primitive/binary64 域/与声明类型匹配），`BoundedNode::Bool` 构造时丢弃允许集，validate 只查布尔类型。同类：无 `type` 的嵌套节点上的 `pattern`/`properties`/`required`/`items`/`min…`/`minimum`/`additionalProperties` 被读取后节点退成 unconstrained `Any`，工具留在 surface 上宣传一条没有执行语义的规则。

**修复**（`crates/agent-contracts/src/schema_profile.rs`）：编译接受的约束要么保留可执行语义、要么在 admission 用类型化错误显式拒绝，不存在第三条路径。

- `BoundedNode::Bool`/`BoundedNode::Null` 增加 `enum_options` 字段；validate 先 `expect` 类型再 `check_enum`（:435-:447、:409-:412、:455-:456 构造点）。布尔枚举是真实的收窄（`[false]` vs 全体布尔），此前是纯静默弱化。
- `enum: []` 编译失败（JSON Schema 2020-12 meta-schema 要求非空；空选项集不接受任何值，是 producer bug，在 admission 点名而不是编译成永远拒绝的门）。
- 无 `type` 节点新增 admission 守卫：只支持 `enum`（走既有 `Enum` 节点，可执行）加注解（`description`）；任何 shape 约束关键字命中即编译失败，错误点名关键字并要求声明类型（:362-:391）。空 `properties: {}`/`required: []` 语义上不约束任何东西，不拒绝。
- 模块文档新增 "Constraint survival" 一节：空 enum、注解 vs 约束、无 type 节点三类明确分类；编译 profile、渲染给模型的 schema、dispatcher gate 三者同义（编译失败的方案在 `surface.rs::compile_schema_profiles` 被 omit 并记录 diagnostics，MustSurface 工具升级为 round block——该 fail-closed admission 机制已存在，本轮让"接受却丢弃"不再绕过它）。
- **爆炸半径核实**：对 `crates/tool-runtime/src` 全部工具 schema 做静态扫描（typeless 属性节点带约束关键字），零命中；本轮后 `cargo test -p agent-core`（含依赖真实工具集的 kernel/surface 路径）全绿。外部 MCP 工具若声明此类 schema 将被 admission 拒绝并出现在 `schema_rejected` diagnostics——这正是审查要求的显式拒绝。

**红-first 回归**（契约层 `schema_profile.rs` tests、Core 层 `kernel/tests.rs`）：

- 契约层 3 用例在修复前失败：`boolean_enum_survives_compile_into_validation`（`flag=true` 通过校验）、`typeless_constraint_keywords_fail_compilation`（SCHEMA_CASES 的 `nested_typeless_pattern`/`nested_typeless_required` 及 6 个同类 typeless 约束全部编译成功）、`empty_enum_fails_compilation`（旧代码编译出 `Enum { options: [] }`）。修复前实测：`19 passed; 3 failed`。`typeless_enum_only_nodes_stay_executable` 为分类钉子（typeless enum-only 保持可执行），直接绿。
- Core 层 `boolean_enum_mismatch_refuses_before_approval_or_dispatch` 修复前失败——`PanicDispatcher` panic，证明 `flag=true` 此前真的到达 dispatch；断言审批计数为 0、无 lease、无 effect row。合法对照 `boolean_enum_valid_argument_reaches_approval_and_dispatch`（`flag=false` 到达审批＋dispatch，计数 1）修复前后均绿。

**变异恢复验证：** 把 `NodeType::Bool => BoundedNode::Bool { enum_options }` 临时还原为丢弃（`enum_options: None`）→ 契约层 `boolean_enum_survives_compile_into_validation` FAILED（21 passed/1 failed）、Core 层 `boolean_enum_mismatch…` FAILED（dispatch 被触发）→ 恢复后全绿。承重确认。

## F4（`d84d6ee8`）：JCS 小数指数边界错误

**用户故障：** `scientific_from_ryu` 的前导零窗口 `(-6..0)` 错误包含 n=-6（应为 −6 < n ≤ 0），Ryu 对 `1e-7` 的最短形式被改写为 `0.0000001`——本实现产出的 canonical bytes 与所有跨语言 JCS 实现不同，其 `ArgumentDigest` 因此偏离模块文档承诺的跨语言一致摘要（与 MECHANISM_CHECKS.json 的 Node 实测一致：3/8 样本字节不同）。

**修复**（`crates/agent-contracts/src/jcs.rs::scientific_from_ryu`）：四个分支逐一对应 ECMA-262 `Number::toString`（RFC 8785 §3.2.2.3）：`k ≤ n ≤ 21` 补零、`0 < n < k` 插入小数点、`−6 < n ≤ 0` 前缀 `0.`、其余指数形式 exp=n−1。旧代码第一分支的 `point >= k` 与 `(0..=21)` 冗余条件也归位为 `(k..=21)`/`(1..=21)`，消除 n=0 落入"小数点插入"分支产出 `.5` 形态的潜在路径。

**回归**：

- `sub_unit_numbers_stay_scientific_across_the_1e6_threshold`：±1e-7、±1.2e-7、1.5e-7、9.9e-7、1e-8、1e-20（拒绝侧）与 1e-6、1.5e-6、1e-5、0.5（合法对照）。修复前实测 `6 passed; 1 failed`（首例 `value 1e-7` 即失败）。
- `exponential_threshold_at_1e21_stays_on_the_spec_side`：9.999e20/9e20/1e20 对 1.5e21/1e22/1e21 两侧。该用例与完整 Appendix B 向量在修复前即绿（Appendix B 不含 n=−6 band 的样本——如实记录：转红由阈值用例单独承担）。
- `rfc8785_appendix_b_vectors`：RFC 8785 Appendix B 全部 32 条 bit-pattern 向量；期望值逐条由本机 Node（v24.14.0）`JSON.stringify` 现场生成（手写草案中的 0x40ac…/0x000000000000000f/0x41b3de… 三条与实测不符，已按实测修正）。
- `operation.rs::sub_unit_scientific_numbers_digest_from_the_contract_bytes`：钉住 `ArgumentDigest::from_json(json!(1e-7))` 等于 `sha256_bytes(b"1e-7")`（合同字节），且与 1e-6 摘要不同。
- **有限浮点 bit-pattern 采样差分**：开发期一次性用例（已删除）输出 11,995 个确定性 xorshift 采样（8,000 全域均匀 + ~4,000 偏置到 1e-7 band 的指数域），经 Node `JSON.stringify` 逐条对照，**0 mismatch**。采样为有限验证，非全空间证明。

**变异恢复验证：** 把 `(-5..=0)` 临时还原为旧边界 `(-6..0)` → `sub_unit_numbers_stay_scientific_across_the_1e6_threshold` FAILED（6 passed/1 failed）→ 恢复后全绿。承重确认。

## ArgumentDigest 历史兼容策略（沿 E2 模式）

- 摘要构造（SHA-256 over canonical bytes）不变；受影响值域仅为最短十进制渲染位于 n=−6 band 的数字（1e-7 ≤ |v| < 1e-6 及 −1e-6 < |v| ≤ −1e-7），其余全部参数与修复前逐字节同摘要。E2 的整数域规则（±2^53 admission 拒绝）原样保留，未重开。
- 摘要每次调用从内存参数派生一次（identity 派生点），之后作为不透明 32 字节身份贯穿审批/发布/执行/恢复；`execute_published_tool` 的 identity fence 重算摘要时对比的是同一进程同一代码版本派生的两侧。恢复路径（`kernel/operation.rs::recover` 等）重放持久摘要字节，**不从持久参数重算**。
- 因此：历史 WAL/记录无需批量重算、也不重算；校验不放宽。旧 band 字节是本地实现偏离合同，不是合同变更。唯一跨版本边界：若未来新增"用持久参数重算摘要并与持久 identity 对比"的路径，旧 band 记录会 fail-closed 拒绝（identity fence 报 mismatch，类型化拒绝，不误执行），届时需显式迁移设计；当前代码不存在该路径。

## 验证汇总（本地 Windows，真实执行，共享 `CARGO_TARGET_DIR=/d/Users/Ye_Luo/APP/cap-agent-target-b`）

| 命令 | 修复前 | 修复后 |
|---|---|---|
| `cargo test -p agent-contracts schema_profile` | 19 passed / **3 failed**（F3 红） | 22 passed / 0 failed |
| `cargo test -p agent-core boolean_enum` | **1 failed**（dispatch 触发）/ 1 passed | 2 passed |
| `cargo test -p agent-contracts jcs` | 6 passed / **1 failed**（F4 红） | 7 passed / 0 failed |
| `cargo test -p agent-contracts`（全量） | —（红阶段按过滤执行） | **198 passed** / 0 failed（基线 190 ＋ F3 4 ＋ F4 4） |
| `cargo test -p agent-core`（全量） | —（红阶段按过滤执行） | lib **157** ＋ 12 ＋ 3，全绿（基线 lib 155 ＋ F3 2） |
| `cargo fmt`（agent-contracts、agent-core） | — | 执行 |
| `cargo clippy -p agent-contracts --all-targets` | — | 0 警告 |
| `cargo clippy -p agent-core --all-targets` | — | 0 警告 |
| Node 差分（开发期） | 旧实现 vs Node：n=−6 band 字节不同 | 11,995 采样 0 mismatch |

未触碰：`Cargo.toml`/`Cargo.lock`（Ryu 1.0.23 未动）、`.github/workflows`、`docs/CURRENT.md`/`docs/NEXT_TASKS.md`、E2 已修的非精确整数/Any 深度/数值域规则。未扩张成完整 JSON Schema 实现。

## 剩余限制

- F3 的 typeless 拒绝是 admission 收紧：此前静默接受 typeless 约束的工具（若存在）升级后会被移出 surface 并记录 diagnostics；内部工具静态扫描零命中，外部 MCP 工具未逐一枚举（fail-closed 属审查认可行为）。
- `BoundedNode::Enum`（typeless enum-only）仍是跨类型单一节点；未做每类型细分（无需——选项已过 primitive/binary64/唯一性检查）。
- F4 差分为有限采样；Appendix B 期望值来自本机 Node v24.14.0 实测而非 RFC 原文复制（ECMAScript 语义即规范引用，两个 Node 版本在此语义上一致，MECHANISM_CHECKS 的 v22 对照与 v24 结论相同）。
- 跨版本摘要边界见上文"兼容策略"末条：当前无重算路径；新增此类路径前需先做迁移设计。
