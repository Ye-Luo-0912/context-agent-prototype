# CORE-2：公共上下文义务与保守语义终结（F02、F03）实施回执

日期：2026-09-11。基线 HEAD：`685b6bbb29275bc8ec73ce6625a94567a8b8d23d`。本片对应 [REPORT.md](REPORT.md) F02 / F03 与 [NEXT_STAGE_THREE_TRACKS.md](NEXT_STAGE_THREE_TRACKS.md) §2 CORE-2。**未提交、未推送、未跑远端 CI。**

## 用户现在能做什么

- 改变一项要求（例如日志格式）不会顺便抹掉同一文件上的其他要求（例如 5 秒超时）；只有真正指向旧决策本身的撤销才终结它。
- 用基线上下文引擎（生产默认 `rolling`，以及 `append`）跑任务时，未兑现的强制正文义务会被**如实报告为缺失**，而不是被当成「已满足」。

## F02：决策终结必须指向具体旧决策及替换依据

### 缺陷复现（先失败后修）

新增反例 `crates/context-simple/src/tests/lifecycle.rs`：

```
use AuthService.rs with a 5-second timeout
replace plain-text logging in AuthService.rs with structured logging
```

修复前：超时决策被排入 `Superseded`（`... got Superseded { by: Some(...) }`）——第二条消息的 `replace` 提示与共享实体 `AuthService.rs` 满足了当时的全部条件，但改日志格式并未撤销超时要求。反例先失败后修复转绿。

### 根因

`queue_decision_supersessions` 的 `matches` 只检查「整条消息含替换提示（`has_replacement_cue`）＋ 精确实体相交（`entities_match_exact`）」。F15 已经堵住了**跨任务**重叠与子串近似，但**同任务内**的洞还在：`replace` 的提示作用于**整条消息**，于是「修改该文件某一维度」的消息会终结该文件上的**所有**其他决策。

### 修复：区分「整实体撤销」与「范围化替换」

`crates/context-simple/src/gc/reachability.rs` 新增 `names_the_same_requirement`，`matches` 追加该条件。判据是**提示动词的直接宾语**：

- **整实体撤销**（`has_whole_entity_cue`）——动词宾语就是共享实体，或使用 `instead`/`revert` 这类替换整条旧行的形式：
  - `use AuthService.rs instead`
  - `drop AuthService.rs for the cache layer`
  - `switch to YAML instead of TOML`
- **范围化替换**——动词宾语是实体的某一维度，实体只作介词位置出现：
  - `replace plain-text logging in AuthService.rs with structured logging`（宾语是 `plain-text logging`）

辅以两条更强/更弱的证据：`shares_verbatim_run`（三词以上逐字引用旧行）与「共享非文件路径的实义词」（`timeout`/`logging`/`toml` 等具体维度名词）。文件路径 token 被排除在词比较之外，**路径相同永不单独构成证明**。

外部存储分支（`state.external`）同样接入该判据（用 `context_ref.summary` 作旧文本身份），不再只看实体。

### 保持不变（刻意）

- 精确文件版本替代（`queue_file_body_supersessions`）原样。
- 同 probe 错误修复（`queue_error_verifications`）原样。
- `has_replacement_cue` 与新判据是**与**关系，不放松。

## F03：不允许用空 miss 表示「未支持」

### 缺陷复现

`crates/context-baselines/src/append.rs` 与 `rolling.rs` 的 `materialize` 均硬编码：

```rust
required_misses: Default::default(),   // 永远是空的
```

`agent-host/src/main.rs:133` 的 `None => ContextPolicy::Rolling` 表明**生产默认就是 rolling**。宿主未指定策略时，未实现的强制正文处理被表示成「没有必需正文缺失」，运行时只消费引擎报告的缺失信息，无法识别未兑现义务。

### 修复：基线如实报告 `PromptRequired` 义务未满足

`crates/context-baselines/src/shared.rs` 新增 `required_claim_misses(hints)`：遍历 `hints.anchor_roots`，把每个 `AnchorRootStrength::PromptRequired` 声明报告为 `ContextMaterializationMissReason::Missing`（身份取自声明的 `item_ref` / `source_field_id` / `anchor_revision`）。两个基线引擎改为调用它。

这是**诚实的「不支持」信号**，不是静默成功，也不是去实现 mandatory-claim 解析：

- `SimpleContextEngine`（`dynamic`）本就通过 `realize_required`/`apply_required` 真正兑现该契约，**不动**。
- 运行时侧 `agent-runtime/src/actor/model.rs` 已把非空 `required_misses` 当作 settlement 阻塞（`settlement_candidate && materialized.required_misses.is_empty()`），所以此前「空 miss」会让未兑现义务静默通过——现在会被显式阻断。
- `Recallable` 等其他强度不报告为 miss（引擎可以不搭理它们，不构成义务缺失）。

实验策略差异（append/rolling/dynamic）保持不变；本片只统一**最低正确性边界**。

## 验证

新增回归（均在回退修复的代码上先复现失败）：

| 测试 | 文件 | 证明 |
|---|---|---|
| `a_replace_cue_on_a_shared_file_does_not_withdraw_an_unrelated_requirement` | `tests/lifecycle.rs` | F02 反例：改日志格式不撤销超时要求 |
| `an_explicit_withdrawal_of_the_same_requirement_still_supersedes` | `tests/lifecycle.rs` | 对照组：点名同一要求的显式撤销仍生效 |
| `baselines_report_unsatisfied_required_claims_instead_of_an_empty_miss_set` | `context-baselines/src/lib.rs` | F03：两个基线都报告未满足的 `PromptRequired`；`Recallable` 不报；无声明时不伪造 miss |

已有同族回归保持绿（关键）：`explicit_replacement_supersedes_and_names_the_replacement`（`TOML`→`YAML instead`）、`gc_never_resurrects_superseded_items`、`supersession_reaches_warm_and_stored_decisions`（`drop X for Y`）。

| 检查 | 实际结果 |
|---|---|
| `cargo fmt --all -- --check` | 通过 |
| `cargo clippy -p context-simple -p context-baselines --all-targets` | 0 error、0 warning |
| `context-simple --lib` | **320 通过**，0 失败（基线 318，含本片新增 2 项） |
| `context-baselines --lib` | **17 通过**，0 失败（基线 16，含本片新增 1 项） |
| `cargo check --workspace --all-targets` | 通过 |
| `scripts/doc_consistency.py` | OK（13 live docs） |

## 范围与未验收

- 未跑远端 CI；未提交/推送。
- 未调用真实 provider；未跑真实宿主端到端任务（`rolling` 默认路径下强制正文义务被报告为缺失，但**未在真实长任务上观察运行时如何消费该 miss**）。
- 本片未实现 mandatory-claim 解析进基线引擎——按任务书允许的二选一，选择「引擎明确声明不支持并返回缺失结果」而非「公共层兑现」。
- F02 判据是**启发式但可解释**的（提示宾语位置 + 共享实义词 + 逐字引用），不是完整语义解析；任务书明确禁止为此引入大型语义本体系统。已存在的精确文件版本替代与同 probe 错误修复路径未改。
- 未调整全局打分权重、GC 策略或缓存容量策略。

## 中途回归（如实记录）

首次实现 `names_the_same_requirement` 时只要求「共享非路径实义词」，**过严**，导致两个既有回归变红：

- `gc_never_resurrects_superseded_items`（`use AuthService.rs instead` 只共享路径）
- `supersession_reaches_warm_and_stored_decisions`（`drop CacheStore.rs for the read path`）

据此把判据细化为「提示动词直接宾语 = 共享实体」的整实体撤销识别，两者恢复绿，反例仍被正确拦下。这段过程保留，说明「只在同任务内收紧」必须同时保住已证明合法的撤销形态。

## 下一任务

核心线余下 **CORE-3**（F04：检索—补读—恢复的可信工作流）；平台线并行 B1（PLATFORM-1 精确提交结果查询，F06）/B2（PLATFORM-3 正式连接契约，F05）。
