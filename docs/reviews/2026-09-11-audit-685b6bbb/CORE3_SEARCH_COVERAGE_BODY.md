# CORE-3 首切片（F04）：检索结果完整性长进模型正文——实施回执

- 日期：2026-09-11（工作树，基线 `685b6bbb`，未提交）
- 切片：CORE-3「检索—补读—复用闭环」的 F04 主体；对应 [AUDIT_TODO.md](../../../AUDIT_TODO.md) 2026-09-11 表 F04 行
- 实现落点：`crates/tool-runtime/src/tools/{mod,search,fs,artifact}.rs`、`crates/agent-core/src/kernel/mod.rs`；不改 GUI、不改协议 wire、不改上下文引擎与 GC

## 用户能做什么

模型（以及最终用户）在「有命中」的搜索/列表里也能看到这次扫描是否找全：被跳过多少文件、命中上限是否截断、下一页从哪里继续读、最后一页明确结束。不再出现「一个文件找到了调用点、另一个关键文件根本没扫到，而模型以为证据齐全」的假完成。

## 缺口与修法

**不变式：`TurnFrame` 发给模型的只有 `model_content`；`summary`/`metadata` 到不了请求。** 因此凡是有界或不完整的结果，必须在正文里自带同一条事实，且由类型化扫描值生成（不从 prose 反解）。此前只有零命中路径有正文级警告；F04 指出的正是 positive result ≠ exhaustive result。

1. **共享 helper（`tools/mod.rs`）**：`coverage_footer(clauses)` 生成单行 `[coverage] …` 页脚（空子句返回 None，完整小结果保持朴素正文）；`with_coverage_footer` 追加为正文最后一行。broker 的 head+tail 截断保留尾部，页脚在截断后存活。
2. **`search.grep`**：
   - 新鲜扫描：`scan_incomplete`（命中上限／文件预算／跳过文件计数三类原因）时正文追加 `PARTIAL scan: …; these are not all the matches`（零命中为 `this is not a repo-wide absence`）；溢出续读指针（原 `truncated_note`）并入同一条页脚，不再两条指针。summary 的 PARTIAL 注记与 metadata 原样保留——三处不再互相矛盾。
   - 取消留下的部分命中：正文追加 `scan cancelled after N files; these partial hits are not the complete set`（零命中路径原本就只写 "cancelled"，未动）。
   - 快照分页（`cursor` 页）：中间页正文带 `hits X-Y of N (snapshot <ref>); continue with search.grep cursor=<ref>#<next>`；末页带 `end of results (N total, snapshot <ref>)`——范围身份、快照身份与结束标记全部进入正文。
3. **`fs.list`**：
   - 新鲜扫描：条目预算截断（`scan_incomplete`）时正文追加 `PARTIAL listing: directory entry budget reached; this is not (an empty|the complete) directory (<path>)`——**非空列表同样声明**（此前只有 summary 有）；溢出续读指针并入页脚。
   - 快照分页：同 grep，`entries X-Y of N (snapshot …); continue with fs.list cursor=…`／`end of listing (N total, snapshot …)`。
4. **`artifact.read`**：窗口不是「完整单页全读」时（`has_more` ∨ 扫描预算截断 ∨ 窗口未覆盖全工件），正文追加 `showing lines X-Y of N total[ (scan budget reached; totals are incomplete)]; continue with artifact.read reference=… start_line=Z` 或末页 `end of artifact (N lines)`。完整单页读取保持朴素正文。W06 的诚实 metadata（`total_lines_complete`/`window_truncated`/`has_more`）不变，正文现在与之一致。
5. **`context.search`（内核解析处）**：命中数打满 `limit`（limit=0 表示引擎默认，不构成 cap 证据，已排除）时正文追加 `[coverage] result capped at limit=N; the catalog may hold more matches`，metadata 增 `result_capped`。措辞保持事实陈述，遵守本路径「命中行不写操作说明书」的既有约定。

**checkpoint/分页语义说明：** 分页全部走既有 run-scoped snapshot artifact（digest 定址、不可变），页与页之间不会因源文件/目录变化产生重复或缺口（既有行为）；快照页正文现在携带快照 artifact 身份（历史身份明确）。恢复后同一 run/workspace 的 cursor 仍指向同一 artifact——本切片未为「恢复后翻页」新增专门回归，沿用既有 artifact 存活事实与 `host_restore` 覆盖，如实记录为未验收项。

**明确不做（按任务书）：** 不新建索引/第二套检索栈；不把跨工作区、跨版本负结果当缓存；不改 fs.read 的正文格式（其 `lines={start}-{end}/{N}` 窗口头已是覆盖表达，且是协议正文捕获的输入）；不动全局打分、GC 与缓存容量。

## 回归（8 项新增，除注明外均先在回退修复的代码上复现失败，再随修复转绿）

| 测试 | 覆盖 |
|---|---|
| `search::tests::hits_with_a_skipped_file_declare_incompleteness_in_the_model_body` | F04 反例：命中文件＋超限跳过文件 → 正文必须含 `[coverage] PARTIAL`＋跳过计数 |
| `search::tests::a_cancelled_partial_hit_list_says_the_scan_stopped_in_the_body` | 取消部分命中的正文声明（对 `cancelled_outcome` 的确定性单测） |
| `search::tests::snapshot_pages_carry_identity_and_an_end_marker_in_the_body` | 中间页范围/快照身份＋续读指针；末页 `end of results` |
| `fs::tests::a_partial_nonempty_listing_declares_incompleteness_in_the_model_body` | 非空 PARTIAL 列表的正文声明＋路径范围 |
| `artifact::tests::the_body_names_the_window_and_the_next_page` | 窗口/总数/续读指针入正文；完整单页保持朴素；末页 `end of artifact` |
| `artifact::tests::a_scan_budget_truncated_artifact_declares_incomplete_totals_in_the_body` | 8 MiB 扫描预算截断 → 正文声明 totals incomplete |
| `kernel::tests::a_capped_catalog_search_says_the_cap_in_the_body`（含未触顶对照组） | context.search 打满 limit 的正文 cap 声明＋`result_capped` metadata；对照组无页脚。此测试先在 HEAD `kernel/mod.rs` 上单独复现失败（临时仅回退实现文件验证），再随实现转绿 |

另修准 2 个既有断言（行为有意变更，意图保留）：`grep_pages_a_consistent_snapshot`／`fs_list_pages_a_consistent_snapshot` 的页行数计入页脚，并补页脚内容断言；W06 `long_first_line_truncates_at_the_cap_without_merging_lines` 的「1 行渲染」改为「1 内容行＋1 页脚行」，反合并断言（截断尾不与下一行拼接）原样保留并加强。

## 增量：checkpoint 半——恢复走查抓到真缺陷并修复（同日）

回执初版把「恢复后快照分页」列为未验收。随后补做的真实组合走查**抓到一个真缺陷**：冷恢复后运行时 run id 换新，而 `Workspace::open_artifact_for_run` 严格校验引用里的 run 段——**恢复前捕获的全部 artifact 引用（grep/fs.list 快照 cursor、spill 引用）在恢复后被拒**（`artifact reference does not belong to run …`），「能继续有界补读」在正式恢复路径上断裂。该回归先在「HEAD＋本切片、无修复」树上复现失败，随修复转绿。

**修复（最小授权面，两个落点）：**
- `agent-workspace/src/lib.rs`：新增 `admit_artifact_run_lineage(current, predecessor)`——前代 run（并继承其自身谱系，链有界 32、最旧先淘汰）持久化到 `.focus-agent/artifacts/<current>/lineage.json`（temp+rename 原子写；state 目录为运行时持有，模型可写面无法伪造）；`open_artifact_for_run` 仅在 run 不匹配且前代在谱系内时放行——**sealed digest 校验与 confinement 照旧，谱系从不放宽内容校验；谱系文件缺失/损坏/超界一律 fail closed**。任务完成证据校验（`task.rs` 的 `artifact_relative_path_for_run`）保持严格：完成证据必须属于当前 run。
- `agent-runtime/src/actor/restore.rs`：`finalize_restore` 在持久提交后经既有 `services.artifact_workspace()` 登记谱系；失败只发 `Warning` 事件——读取保持诚实失败，绝不阻塞或回滚已提交的恢复。

**新增回归（3 项）：**
- `agent-workspace`：`a_restored_run_reads_its_admitted_predecessors_sealed_artifact`（登记前拒绝→登记后放行＋digest 核验→无关 run 仍拒绝）、`run_lineage_composes_transitively_across_restore_chains`（A→B→C 链传递可读＋损坏谱系 fail closed）。
- `agent-compose/tests/core3_restore_snapshot_paging.rs`（真实组合端到端）：120 条命中 → 溢出 grep（正文带 cursor）→ 1 轮预算停落检查点 → 冷恢复新组合 → 用**恢复前捕获的 cursor** 翻到末页（101-120）→ 断言 `end of results (120 total, snapshot <恢复前引用>)`；且两次会话之间源文件已被外部改写，快照页不含改写内容（明确历史身份）、`metadata.hits` 仍为快照总数（未重扫）。

**过程如实记录：** 验证期间共享工作树被并行平台线会话的未提交改动（`platform/work.rs`）临时打断编译；本切片的 agent-runtime/compose 相关验证先在「HEAD＋本切片」的临时 git worktree 完成，主树恢复可编译后全部重跑确认，临时 worktree 已删除。中途两处测试断言错误（末页无 range 子句、页数按 220 算错）由实际运行抓出并修正，非被测代码缺陷。

## 实际检查（最终全量，主树）

- `cargo test -p tool-runtime --lib`：**261 通过**（1 ignored 既有；含新增 6 项回归）
- `cargo test -p agent-core --lib`：**152 通过**（含新增 1 项）
- `cargo test -p agent-workspace --lib`：**108 通过**（含新增 2 项谱系回归）
- `cargo test -p context-simple --lib`：**320 通过**（基线不变，引擎消费端不受页脚影响）
- `cargo test -p context-baselines --lib`：**17 通过**（基线不变）
- `cargo test -p agent-compose`：lib 31＋crash_resume 4＋m16_restore 2＋product_flow 1＋proof_supervision 1＋route_flow 1＋supervision_gate 3＋**core3_restore_snapshot_paging 1** 全绿（`live_walk` 只读 metadata）
- `cargo test -p agent-runtime --lib`：**391 通过**（CORE-1 基线不变；正文捕获只针对 `fs.read`，页脚不进入协议正文/覆盖证明链路——`file_line_range` 与 body window 均取自 metadata/typed 字段）
- `cargo check --workspace --all-targets`：通过，**0 警告**
- `cargo fmt --all -- --check`：通过
- `cargo clippy -p tool-runtime -p agent-core --all-targets`：**0 警告**
- `python scripts/doc_consistency.py`：OK（13 live docs）

## 未验收（如实记录）

- 未提交/未推送、未跑远端 CI。
- 未调用真实 provider；「最终模型请求带不完整状态」由正文构造的单测保证（`model_content` 是 TurnFrame 唯一取用字段已在源码核实），未在真实长任务上观察模型行为。
- CORE-3 本切片（正文完整性＋分页/恢复语义）已收口；其余增量（artifact 分页的平台/GUI 读模型＝PLATFORM-2/GUI-2）不在核心线范围。
- 消费观测共用同一份派生清单的完整收敛仍属 CORE-1 后续与 CORE-4。

## 下一步

核心线 A3（CORE-3）核心侧已收口：正文完整性、分页身份/结束标记、恢复后同一快照语义均有回归把守。按 NEXT_TASKS 波次，核心线下一切片为 CORE-4（缓存与压缩的实际成本优化），在正确性收口后进行；统一用户旅程第 2/3/6 步的对接验收依赖平台/GUI 二波（PLATFORM-2、GUI-2/3）落地。
