# CTX-1 最终可见覆盖清单——实施回执

- 日期：2026-09-12（工作树，基线 `685b6bbb`＋未提交修复，未提交）
- 切片：M18 A 首片 CTX-1（P1）；承接 CORE-1/F01 残余，对应 2026-09-12 审查 [REPORT.md](REPORT.md) E01
- 实现落点：`crates/agent-runtime/src/prompt.rs`（渲染省略）、`crates/context-simple/src/materializer.rs`（descriptor 定价）、`crates/agent-contracts/src/context.rs`（契约文档）、两组测试；无 wire/协议变更

## 用户能做什么

跨窗口读取和继续任务时，模型不会因为「文件版本一样」而漏掉尚未展示的片段：请求里没有可信窗口时，历史正文一律保留（宁可重复、不可丢失）；只有真实的完整窗口区间包含记录区间时才去重。

## 缺口与修法

**E01 逃逸通路**：CORE-1/F01 落了区间感知覆盖（`FileBodyWindow`＋`visible_body_windows_cover`），但两个消费点都保留了「窗口集合为空时退回 identity-only」的兼容分支——`prompt.rs::omit_selected_file_body`（渲染省略）与 `materializer.rs::price_as_file_body_descriptor`（定价）。反例成立：旧 frame/缓存行携带 `path@rev` 但无可信范围（旧 checkpoint、无范围 metadata 的旧 producer），窗口集合为空，identity 命中即把同版本**另一区间**的历史正文省略成 descriptor——模型丢正文。

**修**：两处统一删除 identity-only 分支——`visible_body_windows_cover` 成为唯一省略/定价依据（无 revision、窗口不完整、未知窗口范围、记录未知区间全部返回 false）。语义：未知保持未知；identity 单独永远不构成覆盖证明。`ContextHints.visible_body_identities` 保留（serde default 兼容旧 checkpoint），契约文档改为 **informational only**：runtime 仍从同一趟回注筛选派生并填充（身份仍是清单的一部分），但引擎不得仅凭它省略或重定价。清单仍由最终实际回注/保留的正文派生（CORE-1 已让定价集合=回注集合），不进入第二套持久状态。

**保留（不重做）**：R09 区间包含、F01 裁剪降级、W02 final-pack 范围覆盖、缓存容量策略全部不动。

## 回归

| 测试 | 覆盖 |
|---|---|
| `prompt.rs::an_identity_match_without_any_window_never_omits_the_historical_record`（红-first） | E01 反例：请求带同版本正文但零窗口 → 历史区间保留在最终 working 正文里 |
| `prompt.rs::an_unrelated_files_window_never_covers_a_required_same_revision_record` | 无关文件的窗口不构成覆盖；required 选中记录窗口不覆盖时保留正文（核对最终请求正文，非 helper） |
| `context-simple entity.rs::only_visible_exact_file_body_is_priced_as_a_descriptor`（改写） | identity 无窗口 → 不 descriptor 省略；整文件窗口 → 照旧 descriptor＋reason |
| `entity.rs::fs_read_of_a_descriptorized_body_is_selected_descriptor`（改写） | reread 分类链路改由整文件窗口驱动，原分类语义保留 |
| 既有覆盖回归全部保持：同版本不相交/包含、裁剪窗口不构成证明、缓存命中未回注不贡献窗口、旧 checkpoint 无窗口行解码 complete | 核对点均为最终 `SELECTED WORKING CONTEXT` 消息正文 |

ACK 说明：消费 ACK 记录 materialize 选中项（含 descriptor 省略项——它们确实进入请求），不消费 identity hints；本片无 ACK 行为变化，上述渲染层测试即最终请求正文核对。

## 实际检查（任务书定义的定向范围）

- `cargo test -p agent-runtime --lib prompt::`：**38 通过**（37 基线＋2 新增−1 并入）
- `cargo test -p agent-runtime --lib an_identity_match`：通过
- `cargo test -p context-simple --lib`：**321 通过**（含改写 2 项）
- `cargo fmt --all -- --check`：通过；`cargo clippy -p agent-runtime -p context-simple --all-targets` 的残留告警全部位于 EXEC-1 并行会话在途的 actor/model/services 文件（`OpKind::Materialize`、`context_materialize` 迁移中间态），不归属本片
- 契约无新增字段：旧 checkpoint/旧 JSON 的兼容由既有 serde default 测试保持

## 共享树事实（如实记录）

验证窗口内 EXEC-1 线并行会话正在 `actor/{mod,tools,model}.rs` 落「材料化可取消」（`OpKind::Materialize` 中间态引发 3 项 `restore_tests` 失败与 5 条 clippy 告警，重试窗口内其自行收敛/继续）；本片未触碰任何 actor 文件，按任务书「只在实际改动涉及的 target 上运行」完成定向验证，不对并行中间态做全仓断言。

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；未调用真实 provider。
- 「required 正文被最终裁掉」的 required-miss 路径由既有 A2/W02 回归保持，本片未新增该向用例（渲染层无 required 标记可断言，已以组合回归覆盖「required 记录窗口不覆盖时保留」）。
- GUI 成本/新鲜度消费面、EXEC/COST 各片按各自线推进。

## 下一步

A 线 CTX-2（明确撤销同一个要求）；与 B（EXEC-1）/C（COST-1）首片并行不互斥。
