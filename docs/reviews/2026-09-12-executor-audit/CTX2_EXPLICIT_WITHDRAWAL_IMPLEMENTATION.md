# CTX-2 明确撤销同一个要求——实施回执

- 日期：2026-09-12（工作树，基线 `685b6bbb`＋未提交修复，未提交）
- 切片：M18 A 线第二片 CTX-2（P1）；承接 CORE-2/F02，对应 2026-09-12 审查 [REPORT.md](REPORT.md) E02
- 实现落点：`crates/context-simple/src/gc/reachability.rs`（判定重写）、`crates/context-simple/src/tests/lifecycle.rs`（5 项新回归）、`crates/context-simple/src/gc/full/tests.rs`（1 项触发形式改写）；无契约/wire 变更

## 用户能做什么

「改日志格式」这样的补充要求不再撤销同文件上的超时/安全/兼容要求：替代宣告必须**点名它替代什么**，且点名的对象确实是旧决策的一部分；无法证明同一要求时两条并存。既有合法明确撤销（`drop X`、`switch to YAML instead of TOML`、`replace the 5-second timeout with …`）全部保持。

## 缺口与修法（E02 三条泄漏通路）

1. **`instead`/`revert`＋共享实体的整行捷径**：`with structured logging instead of plain-text logging` 的介词结构被当整实体撤销。→ **捷径删除**；对比短语改由 `instead of X` / `rather than X` 的**宾语点名**规则处理——X 短语（≤4 实词）须与旧决策的内容词/实体相交才构成替代（`switch to YAML instead of TOML` 仍撤销：TOML 被点名）。
2. **逐字 run 无内容词**：`use AuthService.rs with` 这类模板开头恰好构成 3 词逐字 run，被当「引用旧行」。→ `shares_content_run`：run 必须含至少一个非停用、非路径内容词。且消息**逐字包含旧行全文**时是重申/超集而非引用替代（`contains_verbatim` 保护，重申不撤销；对象点名规则仍适用）。
3. **共享任意实词分支**（CORE-2 的最弱证据）→ **删除**，由「替代宾语点名」取代——通用同词启发式不再给语义死亡授权。

**新增保留保护**：消息含保留/否定词（keep/retain/preserve/remains/still/never/not/don't/do not）→ 整体不替代（歧义先并存）。

**行为收紧（按 E02 授权，如实记录）**：`use AuthService.rs instead` 这类裸 instead 形式现归「并存」——它不点名替代对象，无法区分「改用」与「另外也用」；需要撤销时说 `drop X` / `instead of X` / 引用旧行。`gc_never_resurrects_superseded_items` 的触发形式随之改写为 `drop AuthService.rs`（测试本意——GC 不复活 superseded——保持）。

判定链（可解释、单一原则）：保留保护 → 逐字内容 run 引用（排除重申超集）→ 替代宾语点名（replace 宾语 / instead-of / rather-than 对比宾语）→ 直接宾语撤回动词（drop/remove/switch/discard/abandon＋实体为宾语）→ 其余一律并存。Stored/Warm/External 与 Resident 走同一函数（外部分支以 stored summary 为准——摘要不足不以摘要前缀作终态依据的边界保持）。

## 回归（5 项新增＋2 项改写）

| 测试 | 覆盖 |
|---|---|
| `an_instead_of_phrase_names_its_own_object_not_the_whole_file`（红-first） | E02 双消息反例：旧超时决策保持 live |
| `one_message_with_several_requirements_restates_without_revoking`（红-first） | 一句多条要求/重申不撤销 |
| `negated_or_retaining_wording_never_supersedes`（红-first） | do not replace / keep → 并存 |
| `a_stored_decision_gets_the_same_scoped_instead_of_rule`（红-first） | Stored（外存摘要）同一判据，决策保持 live |
| `a_chinese_appended_condition_coexists` | 中文追加条件并存（中文撤销识别不在本片，如实记录为限制） |
| `gc_never_resurrects_superseded_items`（改写触发形式） | 明确撤销形式下 GC 不复活语义保持 |
| 既有 `later_decision_supersedes` / `explicit_replacement` / `a_replace_cue…` / `an_explicit_withdrawal…` / `compatible_decisions…` / `cross_task…` 全部保持 | 真正撤销成功、跨 task 无影响、兼容共存 |

## 实际检查（任务书定义的定向范围）

- `cargo test -p context-simple --lib`：**327 通过**（基线 321＋5 新增＋改写 2 项计入）
- `cargo fmt --all -- --check`：通过；`cargo clippy -p context-simple --all-targets`：0 警告
- restore 侧：GC/语义终态相关 restore 测试含于全量（无独立 restore 回归需要变更）

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；未调用真实 provider。
- 中文撤销识别不支持（中文无空格分词，追加条件类并存正确）；`use X instead` 裸形式从撤销降级为并存——两个边界均为 E02 授权内的语义收紧，非缺陷。
- 共享树窗口内 EXEC-1 并行会话在途 actor 改动照旧不在本片验证范围。

## 下一步

A 线 CTX-3（摘要来源、覆盖与失败身份）；B（EXEC-1）/C（COST-1）首片由各自线推进。
