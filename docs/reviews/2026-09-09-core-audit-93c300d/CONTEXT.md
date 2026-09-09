# Context / GC / 搜索：所有权、覆盖范围与恢复根

基线 `93c300d9b222ea9720579b86ac273e945f1964bc`。以下四项均有隔离 ContextEngine API 反例；不是完整宿主断电或真实模型评测。现存源码在 [evidence/context-probe](evidence/context-probe/Cargo.toml)，执行记录见 [EVIDENCE.md](EVIDENCE.md)。

## R02 / P1：待外置重试记录未进入 Storage GC 强引用根

位置：`crates/context-simple/src/store.rs:1026–1036`；同一所有权遗漏的进展问题在 `gc/full/mod.rs:91–99`。

A1 已用 `State.pending_externalize_retry` 保留外置失败/取消时的原始记录；`gc/full/mod.rs:279–293` 把溢出记录移入它，IO 成功后再交接所有权。问题是后续遍历仍有部分代码只认识 heap、warm、external：Storage GC 只从 heap/warm 收集强边，漏掉 pending 的有效引用。仅剩 pending 时，full GC 的空状态判断还会直接返回，导致重试永久不再发生；inspect 的目录投影也看不到该记录。

探针先生成真实磁盘证据 blob，再构造与当前 GC 故障窗口一致的合法序列化 owner 状态。对照组与实验组的记录、语义、强边完全相同，仅所有权位置不同：

| 状态 | Storage GC 删除数 | 结果 |
|---|---:|---|
| Live owner 在 heap，DerivedFrom 指向旧证据 | 0 | 证据保留 |
| 同一 Live owner 在 pending_externalize_retry | 1 | 证据文件实际消失，owner 仍存在 |
| 此后仅剩 pending，再运行 GC | externalized = 0 | pending = 1，catalog rows = 0 |

动态探针验证了真实文件删除和重试停滞；pending 状态通过 checkpoint 构造，没有注入实际磁盘故障来驱动整个 Runtime。生成证据时使用 buffer capacity 0；实际删除对照使用默认 GC 配置，结论并非只能在零容量配置下触发。

令 `R = heap ∪ warm ∪ pending ∪ retained_external ∪ anchor_roots`。必要条件是：

```text
Delete ∩ Reach_strong(R) = ∅
pending ≠ ∅ ∧ IO最终可用  ⇒  后续维护能够重试
```

第一条是安全性质，第二条是活性性质；本例两者分别失败。重试队列保存了对象，不代表所有权已经接进所有消费者。

修复方向：复用一套完整 owner 遍历，覆盖强引用根、checkpoint 校验、目录可见性与维护入口；保留失败时的正文所有权。不要靠把 pending 塞回已满 warm、无限扩大缓冲或修改热度算法掩盖问题。必要回归应固定 GC 外置失败/取消窗口，随后 Storage GC，再恢复 IO 并确认重试成功。

## R03 / P1：恢复清理忽略仍受支持的旧 checkpoint 根

位置：`crates/context-simple/src/store.rs:1439–1450`、`engine.rs:1420`。Runtime 存储默认保留 32 份 checkpoint、总预算 64 MiB（`agent-runtime/src/checkpoint.rs:105–113`）。

实际顺序：

1. 外置正文，保存 A：A 只有该正文的 Cold 引用。
2. 通过真实 Admit 将正文放回 Resident，保存 B。
3. restore B 后 reconcile 看到“当前没有 external owner、同 ID 已 resident”，删除磁盘 blob，`deleted_stale = 1`。
4. restore 仍保留的 A 成功，但 `fetch_external(id)` 返回 None。

探针使用真实 blob IO 与 ContextEngine checkpoint/restore/Admit/reconcile API；A/B 是保留在测试进程中的 checkpoint 值，未做完整 CheckpointStore 文件及宿主恢复实验。产品可达性由当前 Runtime 代码和既有测试确认：恢复验证的是合法 authority ancestor，不要求最新 checkpoint；`tests/instance/restore.rs:163–189` 明确允许 authority epoch/sequence 前进后恢复旧快照，`load_verified` 也不与 latest 比较。

因此不能以“B 里还有 RAM 正文”证明 blob 可删除。必须把支持恢复的 checkpoint 也算作所有者：

```text
RecoveryRoots = ⋃ { BodyRefs(C) | C仍被保留且允许恢复 }
Delete ∩ Reach_strong(CurrentRoots ∪ RecoveryRoots) = ∅
```

修复方向：沿现有 checkpoint 保留机制，将可恢复正文引用纳入删除判定，或明确而原子地结束旧 checkpoint 的恢复承诺后再释放正文。不能让 reconcile 以当前视图里的重复项为由绕过 Storage GC 删除规则；不需要新增 trace 数据库。必要回归应真正保存 A/B 文件、恢复 B 并对账，然后通过正式信封恢复 A 并读取正文。

这不是重报已修的“Admit 立即删除 blob”：当前 Admit 已保留 blob，删除发生在后续 reconcile，缺的是跨 checkpoint 的所有权。

## R08 / P2：Rolling 把未进入压缩输入的正文也标成已覆盖

位置：`crates/context-baselines/src/rolling.rs:156–180`，覆盖标记在 `:265–272`。

算法先按 token 数选择要移走的完整 records，然后逐条移除；压缩输入只有前 2,000 字符，成功后整批 records 都退出工作集。最新 A3 正确地把旧摘要放在前面，但没有解决本轮新记录超出输入上限的部分。摘要文案仍宣称 covers 全部 folded records。

默认 RollingConfig 的探针输入为一条 5,000 多字符的旧消息，以及两条各 16,000 字符的新消息。旧消息末尾放置唯一标记。实际维护 folded_records = 1，compactor 只收到 2,000 字符；标记既没进入 compactor，也不在随后 engine checkpoint。测试 compactor 只返回“所给前缀的摘要”，因此反例不依赖模型是否会忘记信息。正式 host 的默认策略确实为 Rolling（`agent-host/src/main.rs:118`），compose 可以注入真实有界压缩器。

这项评为 P2：摘要本来就是有损工作集视图，原始历史/工件可能在别处保留，本轮没有证明全系统永久丢失用户数据。确认的问题是**未经压缩器阅读的部分被移除，同时覆盖说明过强，Rolling 记录也没有给出这段残余的可恢复引用**。

令 `F` 为移出正文，`I` 为实际摘要输入，`U` 为保留或可按引用恢复的残余。需要 `F ⊆ I ∪ U`，覆盖说明也应区分完整与部分输入。摘要的语义质量不能被当作严格无损数学证明。

修复方向：按输入容量切分消费记录，保留未消费后缀/记录，或生成真实可恢复引用及 partial coverage；不要简单提高字符上限或重新训练打分。必要测试要核对 compactor 实际收到的源范围、未消费残余和报告，而不只看摘要非空/总 token 下降。

## R09 / P2：同文件版本的不同读取区间被误当作相同正文

位置：`crates/agent-runtime/src/prompt.rs:556–580` 生成 hint；`crates/context-simple/src/materializer.rs:836–850` 消费；Runtime 历史正文省略逻辑另在 `prompt.rs:1007–1026`。

`fs.read` 为分段读取保留整文件 revision，另带 start_line/end_line/covers_file（`tool-runtime/src/tools/fs.rs:468,507–516`）。但 Runtime 把成功读取变成仅有 `path@revision` 的 visible body identity，Context 只据此将历史正文替换成描述符。

探针导入含 100 行的历史 L1–100 正文，确保唯一约束处于开头。正常 materialize 能看到它；提供 Runtime 对当前 L101–200 会生成的同版本 hint 后，正文消失，结果只剩 `src/a.rs@rev-1`，选择理由称 exact body already visible。动态验证的是公开 Context API；Runtime 丢失范围的生成路径是源码确认，没有运行完整模型轮。

必要条件不是版本相等，而是：

```text
same(path, revision) ∧ historical_interval ⊆ union(visible_intervals)
```

未知范围不能作为完整覆盖证明。A2 已修复截断后的 range/partial 字段，但这两个消费者仍没有使用范围。

修复方向：共享契约由单一维护者补足范围/完整性信息，prompt 去重、Context descriptor 和最终预算删减的 required miss 判断消费同一覆盖规则。不要让各模块各自拼字符串猜完整性。必要回归包括不交叠区间、部分交叠、完整覆盖、未知范围以及最终 packing 后的可见范围。

## 搜索与算法判断

当前已读的 shared catalog / inverted index、stored body 补读、tool search 路径，没有再确认需要替换索引的新缺陷。Stored 搜索单次最多 256 个正文读取，每个 blob 有 1 MiB 上界；需要超过读取预算时明确要求收窄查询，读取失败也没有包装成完整零命中。这些是当前源码结论，不是性能评测结果。

继续保留索引是合理方向。先定义查询究竟搜索身份、摘要还是正文，并保证候选生成对该语义不漏掉允许范围内的匹配；然后在 `|reads| ≤ B_reads`、`bytes ≤ B_bytes`、结果数量与 deadline 下执行。只有全部候选已被充分检查，才能报告完整无匹配；预算用完、坏 blob、候选被裁掉需要可见的 incomplete 原因。

数学上，候选完备性与排序质量是两个问题：高分排序不能修复候选阶段的漏召回，向量库也不能修复正文覆盖身份、所有权或恢复根。优化应先计量 catalog 扫描数、候选数、正文读取次数/字节与耗时；没有基准证据，不声称当前检索延迟或召回率已达标，不重开冻结研究。
