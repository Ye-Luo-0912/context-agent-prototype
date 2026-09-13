# M18 A 线 CTX-5..9 实施回执（2026-09-13，工作树）

对应第二轮审查（基线 `685b6bbb` 加未提交树）R2-03～R2-07 的五个切片，A 线（上下文/GC/长期证据）全部代码落地。每片先以红检查证明测试捕获旧行为，再随修复转绿；共新增回归 **29 项**、改写 2 项固化旧行为的既有断言。**未提交/未推送、未跑远端 CI；真实 provider 照旧 NOT_RUN。**

## 实际执行的验证（本机 Windows）

| 目标 | 结果 |
|---|---|
| `cargo test -p context-simple --lib` | **363/363**（基线 336 → 含本片 29 项新增；红检查逐片先复现失败） |
| `cargo test -p agent-contracts --lib` | **174/174**（新增字段均为 serde default，旧 JSON 兼容回归保持） |
| `cargo test -p context-baselines` | **23/23** |
| `cargo test -p agent-compose`（12 目标） | 全绿（含引擎级端到端走查） |
| `cargo check -p agent-runtime --lib` | 通过 |
| `cargo fmt -p context-simple -p agent-contracts -- --check` / `clippy --all-targets` | 0 diff / **0 警告** |

**共享树并行域中间态（如实记录，不归属本片）：** 并行会话正在 contracts 上落地 `TaskCompleted{artifacts, final_output_digest}`，其自身测试构造（agent-runtime `status.rs` 2 处、agent-replay 3 处、agent-eval 3 处）尚未收口，`--all-targets` 检查在这些文件报 E0063；runtime/eval 的 **lib 目标**编译通过。本片未触碰这些文件。共享 contracts 新字段由集成者在 `agent-eval/metrics.rs` 同步收口（284 行并行改动）。

## CTX-5（R2-03/P1）：Live 必需正文保护贯穿老化与语义终结

**唯一 live 保护判定** `engine::anchor_claim_defers_expiry(state, item)`：当前投影（`state.anchor_roots`，运行时每次 GC/materialize 前整集替换、释放＝空投影，旧/空投影不冒充当前根）中一条 ResidentRequired/PromptRequired 声明命中的 *live* 条目，被四条启发式终结路径一致豁免——驻留 ephemeral TTL 与 ttl×4（`residency.rs` 新参数）、warm 缓冲老化（`minor.rs`，按 pass 预计算受保护 id 集）、full sweep 的普通对话老化/关闭 scope 成员/已消费 ephemeral 释放（`full/mod.rs` `alive_root` 重构：语义死亡仍最先获胜，保护绝不复活）。命令驱动的召回不再被启发式预算饿死：warm 权威召回与 Stored 声明召回各有独立预算（上限＝`MAX_ANCHOR_ROOT_CLAIMS`）。报告新增 `anchor_root_misses`（当前投影中未持有任何 live 正文的法律义务显式上报，serde default）。**回归 7 项**（`tests/anchor_expiry.rs`）：全链 claim→gc→heap 存活、PromptRequired ephemeral 跨 TTL 存活且正文到达最终帧（UserInput 触发直达 TTL 分支，红检查证明 AfterModel 的 consumed 分支会掩盖它）、warm 不被老化终结、终态不复活、释放后正常老化、StorageRequired 不延长驻留、报告点名未满足声明。红检查：短路保护判定后 4 项核心测试全红。

## CTX-6（R2-04/P1）：Pending 是完整 owner

`pending_externalize_retry`（store 写失败时的内存全正文 owner）接入全部语义与材料化路径，统一按 item_id 解析唯一 owner：①终态——`queue_decision_supersessions`/`queue_error_verifications`/`queue_file_body_supersessions`/`queue_error_recurrence` 四个队列扫描加 Pending；`apply_terminal_semantic` 与 `has_matching_verification_evidence` 加 Pending 臂（排队意图真正落地，body 留在重试列表，最终成功的外置写入携带终态语义）；②required——materializer 的 Pinned 与 PromptRequired 查找覆盖 Pending（搜索称它存在、required 回 Missing 的矛盾消除）；③指令——`plan_admit`/`apply_admit`（Pending 像 warm 一样支持 admit、终态拒绝先于迁移、ledger 记 Pending→Resident）、`apply_derive`（Pending 是真实 derive 源）、lease/gc_hint/tag（查找、配额与原地 mutation 覆盖 Pending——控制动作不得静默空转，配额跨正文位置）；④scope close——Pending 的 durable outcome 原地提升（`scope.rs`）。**回归 10 项**（`tests/pending_owner.rs`）：Pending 决策被显式替换、Pending 错误被同 probe 成功验证、Pending 正文满足 PromptRequired、admit 回原 id 恰一 owner、derive 溯源、lease 真实生效且计入任务配额、scope close 原地提升、**真实不可写 store**（父目录为文件）跨 checkpoint 往返恰一 owner、磁盘恢复后 drain 携带当前语义（终态不被外置回滚为 Live）。红检查：7 项语义测试全红（checkpoint 往返与 drain 为既有正确行为守卫）。

## CTX-7（R2-05/P2）：外置读回采用当前 owner 元数据

唯一合并规则 `store::reattach_owner_metadata(entry, item)`：blob 只对内容权威（id/content/kind/实体/依赖边/文件身份/创建时钟）；entry 对一切当前状态权威（task/scope/scope_id/retention/attention/semantic/保护位/访问与打分时钟/世代）。tags 取并集——tags 只增不减，旧 checkpoint 行 entry 侧缺省列表不得抹掉 blob 侧真实标签（含旧生命周期标签，防复活）。entry 的 `scope_id=None` 不构成重打凭证（保留 blob 真实印章而非降级 legacy 推断）。三个读回点全部接入：`fetch_external`、`apply_admit`（外部读臂，合并先于准入重打）、`commit_full_gc` recall（合并后 map 才丢弃条目）。**回归 4 项**（`tests/stored_metadata.rs`），每项走整条移动链（外置→scope close 提升→读回→restore）：fetch/admit/recall 返回提升后的 retention 与 Promoted 标签、内容逐字节不变、创建时钟不变、restore 后再 fetch 一致。红检查：合并退化为原样返回后 4/4 全红。

## CTX-8（R2-06/P2）：故障时总驻留与 full pass 工作预算

两个诚实硬界替换无界积压（新配置 `max_pending_externalize_items`=4096、`gc_externalize_batch`=64）：①重试列表条目上限——满员时溢出**延期**（条目留在有界缓冲，绝不丢弃）并置 `externalize_backpressure`；②单 pass 只序列化/尝试最旧一批 owner（公平 FIFO），单 pass 序列化字节与 store IO 与积压规模解耦。I/O 失败计数进 `store_io_failures`（写/读/panic 三路，不再静默吞掉）；报告新增 `externalize_deferred`；diagnostics 新增 `pending_items`/`pending_bytes`（背压轴可见，不再只看 Warm<cap）。sweep 的 `marked.contains` 从 Vec 改为一次性 HashSet（O(items+marks)，membership-only，输出/选择顺序不变；同一集合复用于 reactivate）。Runtime 背压接线归 B 线（任务书既定）。**回归 4 项**（`tests/gc_backpressure.rs`）：持续故障 6 轮输入下 pending 恰在 cap、backpressure/deferred/pending 字节诚实、零丢失；恢复后按批 FIFO drain（store 文件数逐批 2/2/1）；失败计数恰为尝试批大小；2000 条目无根堆单趟清空且有界。取消安全沿用既有设计（plan 只携带批内预序列化字节，丢弃 IO 至多丢写入不丢条目，既有 EXEC-1/W04/GC-CANCEL 回归覆盖）。

## CTX-9（R2-07/P2）：关闭作用域与历史索引的有界生命周期

可退休集合＝closed 且非 session 且**零直接引用**（heap/buffer/pending/external/active 均不携带其 id）且**全部在树后代皆可退休**（自底向上 Kahn 判定，整条无引用链一趟退休——子树遍历与祖先 close 永不命中缺失节点，无悬空 parent）。退休节点离开树/内存/checkpoint；`RetiredScopeNote`（id/kind/task/closed_tick）进有界事实环（`MAX_RETIRED_SCOPE_NOTES`=512，满淘汰最旧——保留期显式）。**完成事实存续**：`task_completed` 改经 `scope::task_completion_recorded`（在树 closed Task ∪ 退休注记），完成任务不会因节点退休被误判未完成而自动召回；重新打开任务清注记。解除钉链两处：scope close 时非提升成员的外置条目释放 chain stamp（终态/不可提升成员永不参与后续提升，task_id 保留 legacy 推断）；已完成任务的外置条目不再携带 scope stamp（提升边界已过）。配置 `scope_retire_target`=1024（默认远超正常工作集，冻结策略未动）；报告/diagnostics 新增 `scopes_retired`/`retired_scope_notes`。**回归 4 项**（`tests/scope_retirement.rs`）：1 万次工具 scope 开闭后树 ≤128、checkpoint <512KiB、注记环有界；完成任务链退休且完成事实/`task_completed` 存续、外置条目零钉链；退休后完成任务记录不被 hot 实体自动召回；被引用链（含祖先）绝不被退休。红检查：禁用退休后 2 项核心测试红。既有 `external_durable_outcome_is_promoted_on_scope_close` 两处断言钉住旧语义（stamp 保持 Some），按新语义改准为「retention 不提升＋stamp 释放」，提升/转移断言全部保留。**残余（如实记录）**：ExternalMap/Catalog 的全量外置元数据分页/溢出属后续切片（任务书允许的既有 store 恢复引用方向，需 Runtime 背压接线配合）；活动任务内 durable 条目仍钉其已关闭工具帧（有界：每任务 durable 结果数×深度，非无界）。

## 语义边界保持

RuntimeActor/Core 权责、GC 语义终态（保护与退休均不复活终态）、`visible_body_windows_cover` 唯一省略依据、冻结 GC 分数/阈值（active/archive/gc_max_generation 未动）、ContextEngine 可替换性（baselines 23 绿）全部保持。共享 contracts 仅 serde-default 增量字段（旧 JSON/checkpoint 逐字节兼容路径由既有兼容回归覆盖）。


## A 线续接（同日第二段）：任务书缺口收口与等价性能修复

首段五片落地后的定向复核发现并收口三件事：

1. **CTX-7 残余缺口（真实缺陷，红检查在先）**：`materializer::realize_required` 的 required 存储读回把 blob 解码后原样进入最终帧——提升过的条目经 PromptRequired/Pinned 读回时携带外置时的旧 retention/tags（blob 快照），正是任务书点名的「required/foreground stored reads」入口。修复：`RequiredPlanSource::Store` 携带 plan 时的 entry owner 快照（在 op_gate 串行化窗口内与 checksum 同源），读回后经同一 `reattach_owner_metadata` 合并，终态/排除判定改在合并后的条目上（陈旧 Live blob 不能复活 dead owner）。回归 `required_store_read_serves_the_promoted_metadata_not_the_blob_snapshot`：红检查（跳过合并）显示帧携带 Working 而非提升后的 Durable，修复后转绿。至此四个 blob 读回路径（fetch/admit/recall/required）全部走同一合并规则。
2. **完成事实单一来源＋集合化**：报告「优化/验证」清单第 3 条（等价数据结构修复）。新增 `scope::completed_task_facts(state) -> HashSet<TaskId>` 作为完成事实的唯一来源（在树 closed Task ∪ 退休注记），full pass 一次构建、经 `GcPlan.completed_tasks` 共享给 sweep/mark_roots/reactivate/commit 的 stamp 释放——原先逐条目扫描 scope 树的 `O(items × scopes)` 变为 `O(scopes + items)`，输出等价（pass 在集合最后读者之前不改动 scopes）。单 id 辅助 `task_completion_recorded` 委托同一事实集，标记 `#[cfg(test)]`（当前仅测试/审计消费）。
3. **任务书回归缺口补齐**：CTX-9 的「跨很多任务」与「保留 checkpoint 跨退休边界」——`many_tasks_complete_retire_and_survive_a_checkpoint`（12 个任务依次完成退休，树 ≤32、checkpoint/restore 后全部完成事实仍在、树仍有界）；CTX-8 的「大量 roots 的整个 full pass」——`gc_many_roots::a_full_pass_with_many_roots_stays_bounded_and_equivalent`（400 durable session 根＋200 focus 成员，marked/evicted/survivors 精确等价断言）。

**续接段验证：** context-simple **366/366**（新增 3 项）、baselines 23、fmt/clippy 0 警告。**共享树并行域中间态（如实记录，不归属本段）：** agent-runtime `actor/safepoint.rs:595` 存在并行会话在飞的调试行（`DEBUG relay sent` 缺 `.await`）导致 compose 测试目标在该窗口无法编译；agent-eval/agent-replay 的 `TaskCompleted{artifacts,final_output_digest}` E0063 同前。两者均非 A 线文件，本段未触碰。
