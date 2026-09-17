# H1/H2 回执 — Context 语义生命周期（否定保护＋冷页语义意图）

**提交：`d4a0bcbe`**（第十一批，基线 `d3a05d29`，审查：[REVIEW.md](REVIEW.md)）。实施环境 Windows＋cargo 1.97.1，红例先行。

## H1 — 否定形式不再被解释成显式撤销

`crates/context-simple/src/gc/reachability.rs` 的 `has_retention_protection`（216–262 行）：

- 分词后删除 token **内部**撇号（`'` U+0027 与 `’` U+2019）：`don't`/`don’t` → `dont`，进入既有保护名单。
- 补充常见缩写否定：`cant`/`cannot`/`wont`/`doesnt`/`didnt`/`isnt`/`arent`/`wasnt`/`shouldnt`/`couldnt`/`wouldnt`（`not` 已覆盖全部「X not」双词）。裸 `no` 有意不加（「no, remove X」仍是合法撤销）。
- `has_whole_entity_cue` 的整体实体规则、verbatim/content-run/replace-object 证明分支全部未动——「Remove AuthService.rs」正对照保持生效，歧义时保守共存。

红→绿（谓词级＋真实引擎链）：修复前 `assertion failed: has_retention_protection("Don't remove AuthService.rs")`、引擎链报 `Don't remove … must not supersede: ["superseded by decision at turn 2: …"]`；修复后直/弯撇号、Do not、Never、Keep 全部保护，明确 Remove 仍取代。回归：`contracted_negations_with_apostrophes_keep_retention_protection`、`common_contracted_negations_are_protected`、`established_protections_and_removal_controls_are_unchanged`、`contracted_negation_blocks_whole_entity_withdrawal_but_explicit_remove_still_proves`（reachability.rs）、`a_contracted_negation_does_not_supersede_the_decision_it_named`（tests/lifecycle.rs，直撇号＋弯撇号同一用例）。

## H2 — 语义终态不随驻留位置分歧

沿 `PendingColdConsumed` 结构先例（有界持久化环＋安装点落账）：

- `PendingColdSemanticIntent`（reachability.rs 687）：`Supersede{by_id, task_id, entities, content≤4000 chars, reason}` / `Verify{by_id, task_id, probe, reason}`；State 新增 `pending_cold_semantic_intents`（cap 64，serde 持久化，restore 不丢）＋溢出计数 `cold_semantic_intents_dropped`。
- 记录点：UserMessage 臂 `record_cold_supersession_intent`（engine.rs 2766，与 `queue_decision_supersessions` 同门槛：classify_decision＋replacement cue＋实体的存在＋**有 pending 冷卡**才记）；verify.run 成功臂 `record_cold_verification_intent`（engine.rs 2914，task＋probe 俱在）。
- 应用点 `apply_cold_semantic_intents_on_install`（reachability.rs 794）：匹配规则逐条镜像已加载扫描（Decision kind/标签、task 相等、`entities_match_exact`、`names_the_same_requirement_in`；Verify 先过 `has_matching_verification_evidence`）；命中经 `apply_terminal_semantic` 终态化并推入 `pending_ingest_transitions`（下一维护报告可见）；条目已终态则幂等消费；不匹配保留。**三个安装点接线**：批量排空（engine.rs 1693）、按 id 服务（2204）、restore 重水化批（4086）——restore 也是冷卡安装路径，不接它则「Remove X→checkpoint→restore→fetch」复活旧 Live。
- 已核实无需重做：`apply_terminal_semantic` 的 external 分支用 `get_mut`，卡片认领失效、下一 capture 重序列化——已加载位置无复活缺口。

红→绿：修复前五位置用例报 `PendingColdCard: left: Some(Live), right: Some(Superseded { by: … })`（前四位置已绿，唯冷卡复活）；restore 流 `left: Some(Live)`；IoFailed 重试后 `left: Some(Live)`；Verify 流 `left: Some(Live), right: Some(VerifiedFixed{…})`。回归（tests/cold_semantic_intents.rs，真实临时目录＋真实 store）：`explicit_removal_finalizes_the_old_decision_in_all_five_body_locations`（heap/warm/retry/已加载 external/未加载冷卡语义等价）、`a_deferred_cold_supersession_survives_restore_and_never_revives_old_live`、`a_cross_task_cold_removal_keeps_the_old_decision_live_and_the_intent_retained`、`a_transient_card_read_failure_keeps_the_deferred_intent_retryable`（card_read_failure_bomb）、`a_cold_card_error_is_verified_fixed_by_a_matching_probe_after_install`。

## 命令与结果（实际执行）

- `cargo test -p context-simple --lib`：**458 passed / 0 failed**（基线 448＋新增 10；集成人复跑 109.24s 同结果）。
- `cargo clippy -p context-simple --all-targets -- -D warnings`：通过。`cargo fmt -p context-simple`：已执行。
- 集成终验：fmt --all --check 干净；五 crate clippy -D warnings 干净；compose/conformance/workspace/host 全套 0 失败。

## 剩余限制（如实）

- 意图环是有界窗口（64）：溢出丢最旧并计数（`cold_semantic_intents_dropped` 可观测），不是全历史义务清单。
- Supersede 意图的内容副本截断在 4000 chars：要求词保留，超长消息非逐字等价。
- 跨任务不匹配的意图保留到身份匹配的安装发生为止（保守方向，可能长期占一行）。
- 中文等语言的缩写否定/撤销语义不在本片范围（沿用既有审查结论）。
