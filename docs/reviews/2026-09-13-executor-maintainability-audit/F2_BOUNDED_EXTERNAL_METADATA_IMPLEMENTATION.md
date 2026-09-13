# F2 实施回执：capture 有界＋外置冷元数据分页读入

日期：2026-09-13。来源：long-flow backend review 的 F2（scale-bound residual，非已测泄漏）。基线 `7a536468`（main）。A 域文件（`context-simple`），分支 `cursor/f2-bounded-external-metadata-959c`，PR [#6](https://github.com/Ye-Luo-0912/context-agent-prototype/pull/6)。

## 用户结果

外置历史增长时长期运行仍可用：单次 capture 的成本跟随**变化条目数**而不是历史长度；卡片 I/O 不在状态锁内；restore 读入一个有界批次即返回，下一回合的 prompt 组装不需要任何冷元数据；此前能找到的条目仍能找到——按 id 立即可取，搜索/GC 经有界分批 drain 后覆盖不变。**未分页的行是活所有者**：不会因为元数据尚未读入而被当作无主 blob 删除。

## 缺陷（HEAD `7a536468`）

1. `checkpoint()` 在持有 `state` 锁期间做卡片存在性探测与写入——store I/O 与全部上下文工作串行。
2. `external_checkpoint_card_batch` 只约束**新卡片写入**：既有卡片的检查与整条超额尾部的卡片序列化每次 capture 都做一遍，未变化的尾部被反复序列化。
3. `restore()` 返回前读完全部分片卡片，恢复成本跟随总历史。

## 实现

- **`ExternalMap` 卡片目录**：`card_hashes: HashMap<id, hash>`（一个 id ＋12 字符哈希 ≈30 字节，不是元数据）。`push`/`get_mut`/`age_one`/`retain`/`take_all`/`replace_all`/`&mut` 迭代全部作废对应行——`get_mut` 交出可变条目即作废（哪怕调用方只读），因此记录的哈希永远是对**当前**元数据的声明，不可能变陈旧。
- **capture 三阶段**（与 GC/storage GC/reconcile 同形状）：`plan_external_spill` 在锁内只做有界序列化；`run_external_spill_io` 在锁释放后写卡片（内容寻址命中即幂等复用，并把该卡片登记进目录）；第三阶段取新锁登记卡片并序列化 checkpoint，manifest 行只保留活 map 或 pending 目录仍持有的 id。`op_gate` 仍是唯一一致性权威，未新增第二套状态权威。
- **预算**：`external_checkpoint_scan_budget`（单次序列化条目数）、`external_checkpoint_card_bytes`（序列化/写入字节，即 plan 的内存峰值）、`external_checkpoint_io_budget_ms`（锁外批次墙钟）；预算漏下的条目本次保持内联（checkpoint 如实变大），下次 capture 继续。已有卡片的条目零序列化——这正是有界预算仍能收敛的原因。
- **restore 分页**：`external_restore_card_batch` 之外的行留在 `State.pending_external_cards`（`#[serde(skip)]`，capture 时重新写入 manifest——manifest 就是同一份目录）。消费面：`materialize` 不读冷元数据；`fetch_external`/`inspect_external` 按 id 单卡片读入；`search_external`/`gc`/`storage_gc`/`reconcile_store`/context 指令先有界分批 drain（覆盖与删除判定不变）；`diagnostics.total_items` 计入 pending 行（跨 restore 不下跌）。

## 回归（`tests/external_spill.rs` 6 项新增，逐项红检查在先）

| 回归 | 红检查（实测） |
|---|---|
| capture 停在卡片写边界时 `diagnostics` 仍应答（确定性屏障，非时间推断） | I/O 阶段持 `state` 锁 → `Elapsed` 超时 |
| 未变尾部在零写/零字节/零序列化预算下仍全部分片 | 关闭目录查找 → manifest 0 行（应为 20） |
| 序列化预算约束单次 capture 且跨 capture 收敛 `[5,10,15,20]` | 关闭目录 → `[5,5,5,5]`，停在首窗口 |
| 可变句柄作废卡片声明，仅被改条目重新序列化 | 与既有 capture-time 元数据测试互补 |
| 有界 restore 延后尾部：materialize 零冷读、按 id 单卡片读入、search drain 后命中、逻辑总数不下跌 | 忽略批次预算 → 30 全量重水化、pending 0 |
| 分页前 capture 保住每一条延后行（往返不丢） | 忽略批次预算 → pending 断言失败 |
| reconcile 先读入 pending 再分类 blob | 去掉读入 → 20 张延后卡片全被当孤儿删除（blob 紧随） |

## 验证（Rust 1.97.1，CI 钉住的工具链）

- `cargo test -p context-simple`：**384 通过**、0 失败（既有 378 ＋新增 6）。
- `cargo test -p context-baselines`：25 通过（引擎可替换性不变）。
- `cargo clippy -p context-simple --all-targets -- -D warnings`：干净。
- `cargo fmt --all -- --check`、`cargo check --workspace --all-targets`：干净。
- 本机同时以 1.98.1 复跑 384 通过（工具链无关）。

## 支持规模与残余（如实记录）

- 本片有界化：capture 工作量、capture 的锁持有、每批 restore/drain I/O、重复分片成本。**未做**：条目离开内存——`ExternalMap` 仍持有全部外置条目元数据与索引，drain 之后驻留元数据仍是 O(历史)。因此支持规模的诚实表述是「外置元数据能装进进程的历史」，而 checkpoint 字节、capture pass、单批 restore/drain 各自独立有界。
- **唯一残余（CTX-9 内存面）**：真正的驻留分页（条目只存在于卡片、搜索候选来自分页索引）需要既有回执记录的产品决策——每次搜索 `O(spilled)` I/O、新增面向模型的完整性声明、或压缩驻留索引。本片不偷工、不假称已解决。
- 未调用真实 provider、未跑全仓 `cargo test --workspace`（定向包＋全仓 `check --all-targets`）。
- F1（restore spill 所有权 fail-closed，PR [#3](https://github.com/Ye-Luo-0912/context-agent-prototype/pull/3)）为并行独立分支，本片未改其收紧的解析/校验语义；两者在 `engine.rs` restore 与 `tests/external_spill.rs` 上有相邻改动，合并时以后合入者解冲突。

## 远端 CI

- run [`34746807046`](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34746807046)（`30d33bcf`，F2 代码、docs/`merge_paged` 之前）：七 job 全绿，含 `test (windows-latest, part full)`。
- run [`34747095706`](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34747095706)（`f6294702`）：6 绿；Windows full 败于 `agent-eval` 五项 hidden-command。file 断言过；command 为 `python_interpreter_unavailable`（`py -3` / `python3` / `python` 探针均 timeout）。同 SHA 的 ubuntu part 2（含 `-p agent-eval`）全绿。F2 未改 `agent-eval`。CI 已 `setup-python` 3.12 但未把 `AGENT_PYTHON` 指过去。
- run [`34750385596`](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34750385596)（`a4d7a207`，`AGENT_PYTHON` 接到 `setup-python` 的 `python-path`）：6 绿。Windows 步骤日志确认 `AGENT_PYTHON=C:\hostedtoolcache\windows\Python\3.12.10\x64\python.exe`。Windows full 改败于 `agent-compose` `proof_supervision::killing_the_rust_host_cleans_the_exact_proof_tree_without_a_completion_receipt`（`timed out: exact proof tree exit`，10s 门）。此为 CURRENT 已记载的满载抖动类（单独重跑即绿）；F2 未改 compose/宿主监督。workspace `cargo test` 默认 fail-fast，本 run 未跑到 `agent-eval`。不改 `PROBE_TIMEOUT`，不把该超时当 F2 回归。
- run [`34750899552`](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34750899552)（`f2d5e582`）：**七 job 全绿**，含 `test (windows-latest, part full)`。F2 代码在该 SHA 上远端关闭。
- 该绿 run 之后，远端把已合入 main 的 F1/F3/F4 并进本分支（`2077f7ae`：restore 仍先严格解析/拒重复所有权再有界读卡，F1+F2 测试都保留）。合并后 HEAD 的 CI 待本推送；本机 `cargo test -p context-simple --lib` **388/388**（F2 384 + F1 4）。
