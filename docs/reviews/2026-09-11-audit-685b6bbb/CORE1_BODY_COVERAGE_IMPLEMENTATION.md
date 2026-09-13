# CORE-1：正文覆盖一致性（F01）实施回执

日期：2026-09-11。基线 HEAD：`685b6bbb29275bc8ec73ce6625a94567a8b8d23d`。本片对应 [REPORT.md](REPORT.md) F01 / [NEXT_STAGE_THREE_TRACKS.md](NEXT_STAGE_THREE_TRACKS.md) §2 CORE-1。**未提交、未推送、未跑远端 CI。**

## 用户现在能做什么

模型不会再因为读了同一个文件的另一个片段，就把之前需要的片段当成「整文件已可见」而省略掉；只有当确有重复正文进入本轮请求时才去重。

## 起点状态（重要）

接手时工作树**编译不过**。已存在一组**未提交的 F01 半成品**，只做了「写入侧」：

- `agent-contracts/src/context.rs`：`FileBodyWindow` 新增 `complete: bool`（`#[serde(default = "default_true")]` 兼容旧序列化数据）；`visible_body_windows_cover` 加 `!window.complete → continue`（裁剪过的正文不再构成覆盖证明）；配 2 项契约回归。
- `execution/body_cache.rs`：条目改存 `window: Option<FileBodyWindow>`，`EligibleBodyRow { identity, body, window }`，`record()` 收第 4 个 `window` 参数（并校验窗口的 path/revision 与条目身份一致，不符则丢窗口）。
- `actor/mod.rs::record_protocol_body`：已用 `file_read_window_from_output` 取窗口传入。

**未完成的消费侧**导致 3 处 `FileBodyWindow` 构造缺 `complete`、`eligible_protocol_bodies` 仍返回 `Vec<(String, String)>`、`record()` 调用点少第 4 参——`cargo test` 直接 E0063/E0061 失败。

处理方式：补齐消费侧接线，不回退半成品。

> **并列执行说明（如实记录）：**本片由两个执行者并行推进同一 crate 子树。写入侧半成品 + 消费侧接线由执行者 A 完成；执行者 B 同期在 `prompt.rs`/`body_cache.rs`/`context.rs` 上补齐**定价集合与回注集合共用同一筛选**的缺口、缓存层窗口保真回归与同类裁剪断言。两侧在同一工作树上收敛为一份实现（`ProtocolBodyRow` 为唯一行类型，无重复定义）。下方「修改」与「验证」两节为合并后的最终状态。

## 修改

### 单一缓存行类型

`prompt.rs` 新增：

```rust
pub struct ProtocolBodyRow {
    pub identity: String,          // path@revision 身份头
    pub body: String,
    pub window: Option<FileBodyWindow>,  // 这份正文真实暴露的范围与完整性
}
```

`body_cache.rs::eligible_rows` 直接返回 `Vec<ProtocolBodyRow>`（删除局部 `EligibleBodyRow`）。**缓存产出的行就是组装器消费的行**，范围不会在两层之间丢失。

### 覆盖证明只来自真实窗口（F01 核心）

`visible_body_windows_from_parts` 重写。原实现对**每一条**恢复正文硬造：

```rust
start_line: None, end_line: None, covers_file: true   // ← 断言「整文件已可见」
```

现在改为照抄 row 自己的 `window`；`None`（来源未给出可信范围）**不贡献任何窗口**——正文仍作为文本回注（回注资格由身份与 Fresh 事实决定），但绝不升级为 whole-file 覆盖证明。

### 同一条派生清单

以下全部改为 `&[ProtocolBodyRow]`，保证「定价 / 去重 / 预算裁剪 / 消费观测」用同一份可见清单：

`rehydrated_protocol_bodies`、`demanded_file_read_body_rows`（spilled-row 路径改用 `file_read_window_from_output` 取真实窗口，原先直接丢范围）、`visible_body_identities_from_parts`、`assemble_with_catalog`、`assemble_with_catalog_stats`、`visible_body_identities_for_request`、`visible_body_windows_for_request`、`prompt_layer_costs_with_catalog`。

`actor/model.rs::eligible_protocol_bodies` 返回类型随之改为 `Vec<ProtocolBodyRow>`。

**定价集合 ≤ 回注集合（F01 第二处不一致）：**`visible_body_windows_for_request` 原先把调用方传入的全部缓存行计入窗口，而 `visible_body_identities_for_request` 已经过滤。现两者都先跑同一趟 `rehydrated_protocol_bodies(full_turn, &retained, progress, protocol_bodies)`，再各自从 `retained` + 真实回注行派生——**凡未通过 freshness/身份/需求筛选、未真正进入请求的缓存行，既不出现在身份集合里，也不出现在窗口集合里**。调用点 `actor/model.rs` 相应传入 `base_progress_view.as_ref()`。

**裁剪降级（F01 第三处不一致）：**`file_read_window_from_output` 统一抽取窗口；`truncated` 或 `window_truncated` 为真时置 `complete: false` 并清 `covers_file`；完全无可信范围时返回 `None`（不贡献窗口）。`file_read_body_windows` 与 `demanded_file_read_body_rows` 均改走该函数，不再直接信任旧字段。

### 顺带修复

`actor/mod.rs:826` clippy `needless_borrow`（`&touch` → `touch`）。

## 验证

新增回归（`prompt.rs` 测试模块）：

- `rehydrated_body_coverage_comes_from_its_own_window`——同版本 L1–100 与 L501–600 不相交：后段窗口**不能**隐藏前段历史记录（正是报告反例）；自身区间可覆盖；无范围行贡献 0 窗口，不构成 whole-file 证明。
- `a_clipped_rehydrated_window_never_proves_coverage`——`complete: false` 的窗口仍被携带（保留事实）但**不构成覆盖证明**。
- `a_cached_but_unrehydrated_body_proves_no_coverage`——缓存里有正文但未通过回注筛选时，历史记录**不得**被当作已覆盖而省略（端到端 prompt 断言，而非只看缓存内部状态）。
- `a_whole_file_read_still_deduplicates_the_historical_body`——对照项：真实整文件读取仍正常去重，修复只收紧伪造的整文件声明。
- `a_clipped_read_does_not_prove_coverage_of_its_declared_range`——带 `truncated` 的 fs.read 不覆盖其声明区间（broker/runtime 限幅路径）。
- `a_row_that_never_reinjects_contributes_no_window`——**身份集合与窗口集合必须对「哪些正文真的进来了」给出一致答案**（F01 第二处不一致的直接回归）。
- 新增测试辅助 `protocol_body_row(identity, body)`。

契约层新增回归（`agent-contracts/src/context.rs`）：

- `clipped_window_is_never_a_coverage_proof`——`complete: false` 不构成任何区间证明（含被裁剪的 `covers_file` 整体声明）；同区间完整窗口仍有效；裁剪窗口不毒化同区间完整兄弟窗口。
- `legacy_window_decodes_as_complete`——旧序列化 hint 无 `complete` 字段时解码为完整，不静默削弱既有语义；字段正常回环。

缓存层新增回归（`execution/body_cache.rs`）：

- `eligible_rows_carry_the_recorded_window_verbatim`——行携带真实区间，不再压成 `path@digest`。
- `a_window_for_another_identity_is_not_attached`——窗口身份（path/revision）与条目不符时丢弃窗口。
- `unknown_window_stays_unknown_instead_of_becoming_whole_file`——未知范围仍可回注正文，但不产出整文件覆盖声明。

已有同族回归保持绿：`disjoint_window_of_the_same_revision_does_not_erase_the_historical_body`、`containing_window_of_the_same_revision_still_deduplicates`（证明**确有重复时仍能去重**）。

| 检查 | 实际结果 |
|---|---|
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy -p agent-contracts -p agent-runtime --all-targets` | 0 error、0 warning |
| `cargo check --workspace --all-targets` | 通过（所有 `assemble_with_catalog`/`prompt_layer_costs_with_catalog` 调用方随新签名编译） |
| `agent-contracts --lib` | **171 通过**，0 失败 |
| `agent-runtime --lib` | **391 通过**，0 失败（含 `prompt::` 36；基线 385，含本片新增 6 项） |
| `context-simple --lib` | 16 通过，0 失败 |
| `context-baselines --lib` | 318 通过，0 失败（materializer 消费侧 R09 路径） |
| `scripts/doc_consistency.py` | OK（13 live docs） |
| `agent-runtime --test actor` | 76 通过，0 失败 |
| `agent-runtime --test instance` | 31 通过，0 失败 |
| `agent-runtime --test turn` | 131 通过，0 失败 |
| `agent-runtime --test approval` | 4 通过，0 失败 |
| `agent-runtime --test recall` | 3 通过，0 失败 |
| `agent-host`（lib/e2e/restore/config） | 7＋8＋3＋3 通过，0 失败 |
| `agent-platform-protocol` | 42＋10 通过，0 失败 |

合计 **1197 项通过**。这是当前工作树的本地定向验证，**不是全仓结论，也不是新 CI 结论**。

`agent-host`/`agent-platform-protocol` 首跑遇到 Windows `target/debug/incremental` 目录权限竞争（`os error 5`，并行 cargo 写入同一 `target`），清理后以 `CARGO_INCREMENTAL=0` 复跑全绿；本片并行执行期间 `cargo check --workspace` 亦两次撞到同一 `os error 5` 竞争（复跑即过），属环境竞争而非源码回归。注入的 `target/` 构建产物默认被工作树忽略，不影响源码判定。

## 范围与未验收

- 未跑远端 CI；未提交/推送。
- 未验证真实 provider 行为与付费调用；未验证 `context-contextcore` 等远端适配器。
- 本片只修正确性：缓存容量策略、GC、评分与选择策略**均未改动**（符合任务书「先修正确性，容量策略维持现状起步」）。
- W02 的 `record_final_pack_drop` 已消费 R09 覆盖规则；本片让缓存侧提供真实输入后，其反例回归在本轮全量中保持绿（`agent-runtime --lib`）。
- 未覆盖任务书 CORE-1「最小验收」中仍需端到端复核的项：broker/runtime 二次限幅后的覆盖降级在真实 broker 截断场景下的端到端断言（本片在 `file_read_window_from_output` 层已处理 `truncated`/`window_truncated`，但未跑真实 broker 截断链路）。
- 「消费观测共用同一份派生清单」在本片已让**身份集合与窗口集合**共用同一回注筛选；消费侧观测（如最终 `ModelRequest` 的实际正文与声明的一致性抽样）仍留待 CORE-1 后续与 CORE-4。
- 两个执行者并行修改同一 crate 子树（见「并列执行说明」）。合并后为单一实现、单一 `ProtocolBodyRow` 类型，无重复定义；但并行期曾出现 `cargo check`/`cargo test` 因共享 `target` 的 `os error 5` 偶发失败（复跑即过），提交前应复跑一次干净全量以免把环境竞争记为回归。

## 下一任务

按 [NEXT_TASKS.md](../../NEXT_TASKS.md) 平台线队列，第一波余下为 **B1（PLATFORM-1 精确提交结果查询，F06）** 与 **B2（PLATFORM-3 正式连接契约，F05）**；核心线并行推进 CORE-2（F02/F03）。
