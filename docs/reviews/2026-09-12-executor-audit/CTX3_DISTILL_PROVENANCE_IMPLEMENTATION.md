# CTX-3 摘要来源、覆盖与失败身份——实施回执

- 日期：2026-09-12（工作树，基线 `685b6bbb`＋未提交修复，未提交）
- 切片：M18 A 线第三片 CTX-3（P2）；对应 2026-09-12 审查 [REPORT.md](REPORT.md) E04
- 实现落点：`crates/context-simple/src/distill.rs`（来源级装箱＋旧卡参与＋覆盖式替代）、`crates/context-simple/src/engine.rs`（卡片覆盖尾注）、`crates/context-simple/src/tests/{distill,admit}.rs`（6 项新回归＋1 项断言更新）；无 wire/契约变更

## 用户能做什么

episode 工作笔记（卡片）如实声明自己真正涵盖的内容：来源关系（DerivedFrom）只指向压缩器实际读过的条目；预算装不下的来源在卡片上以 `covers N of M sources` 明示排除、原始条目保持可检索；跨 episode 的旧卡不再被无依据终结——卡片是**累计笔记**，新卡把旧卡内容实际装入输入后才替代它；压缩失败时卡片明示「未完成压缩」并附原始来源。

## 缺口与修法（E04 三点）

1. **来源集合大于实际输入**：旧实现先收集最多 8 个 ID 拼正文、再统一截 2000 字符——被截掉的来源仍在 `source_ids`/DerivedFrom。→ **来源级装箱**：成员按最新优先整取或整不取（来源不跨预算边界截半），仅最新的单个超预算来源允许截断进入；`source_ids` 恒等于实际进入输入的来源；被排除成员计数进 `DistillJob.excluded_sources`，卡片头部渲染 `[episode covers N of M sources; earlier sources stay retrievable]`。不增大 MAX_DISTILL_SOURCES、不新增语义摘要权威。
2. **旧卡替代无覆盖绑定**：旧实现输入排除旧卡却无条件 supersede 它。→ **卡片定义为累计笔记**：旧 episode 卡不受 opened_tick 限制、作为普通成员参与装箱；`queue_prior_episode_cards` 只对「实际进入本次输入的旧卡」排队替代（`source_ids.contains`）——替代即有覆盖依据；装不进的旧卡保持 live、不在新卡来源里、可检索。
3. **失败 fallback 伪装完整**：fallback 前缀从裸 `[episode]` 改为 `[episode distill incomplete: compactor unavailable; raw sources follow, still retrievable]`——原文随卡可检索，不声称摘要覆盖。

## 回归（6 项新增＋1 项断言更新；新增项均先红后绿或首跑即证新语义）

| 测试 | 覆盖 |
|---|---|
| `provenance_never_names_sources_the_compactor_did_not_read` | E04 反例：超大来源截断进输入、短来源未读不进 DerivedFrom、`covers 1 of 2` 可见、被排除条目 live |
| `nine_sources_take_the_newest_eight_within_budget` | 9 成员 2000 预算装 7（各 250 字符）、最旧排除且计数可见 |
| `three_rotations_chain_cards_with_coverage` | 三次旋转卡链：新卡 DerivedFrom 含前卡、被覆盖前卡 superseded |
| `a_prior_card_that_does_not_fit_stays_live_and_unsuperseded` | 装不进的旧卡保持 live、无 DerivedFrom 边 |
| `failed_compaction_card_says_it_is_incomplete` | 失败 fallback 明示未完成＋原文随卡 |
| `restore_keeps_provenance_stable` | restore 后 DerivedFrom 数与目标集合不变 |
| `admit.rs::episode_rotation_compact_failure_falls_back…`（断言更新） | fallback 标记随新文案 |

## 实际检查（任务书定义的定向范围）

- `cargo test -p context-simple --lib`：**333 通过**（基线 327＋6）
- `cargo fmt -p context-simple`／`cargo fmt --all -- --check`：本片文件干净（残留 1 处 diff 在 agent-replay，属并行在途文件）
- `cargo clippy -p context-simple --all-targets`：0 警告
- 取消回归：episode 蒸馏取消由 W04 ingest 取消既有覆盖（ingest 内 operation/快照回滚），本片未新增取消机制

## 共享树事实（如实记录）

验证窗口内并行会话在途：EXEC-1（actor 材料化取消，已收敛）、EXEC-3（agent-workspace lineage 加 `protected` 保护集与 `LineageAdmission` 类型化降级——其 fmt 中间态不归属本片）、COST-1（`CompactionOutput`/`ContextCompaction` 增 `cached_input_tokens`/`attempts`/`retries` 字段，本片的调用点已随之适配并透传）。`long_task_10k_turns`/`one_overlong_episode` 既有回归在新装箱语义下保持绿。

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；未调用真实 provider。
- 「旧卡 open loop」场景以旧卡进输入的装箱断言覆盖（脚本 compactor 不做语义摘要）；无 LLM 质量声明。
- COST 侧压缩账目（ContextCompacted 事件身份穿透）归 COST-1 线。

## 下一步

A 线 CTX-4（产品增量：默认长期上下文能力决策，需实际 profile 决策与真实入口验收）；B/C 线按各自队列推进。
