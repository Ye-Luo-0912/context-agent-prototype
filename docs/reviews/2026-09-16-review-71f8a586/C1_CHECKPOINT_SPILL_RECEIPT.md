# C1 回执：Windows CI 上 context-simple 三测试满载失败（checkpoint 外置分片的墙钟预算提前收敛）

分支 `c1-checkpoint-spill`，工作树起点 `a6ee4a6b`（基线 `7631dd72` 的后继，C0 修复已含）。
只改 `crates/context-simple/src/tests/external_spill.rs`、`crates/context-simple/src/tests/fixed_budget_closure.rs` 两个测试文件。
生产代码零改动（engine.rs 的变异仅用于承重验证，已恢复，未提交）。

## 失败事实（run `35158964457`，job `105005847935`，提交 `7631dd72`）

Windows `cargo test (full)` 的 context-simple 测试二进制全程 401.56s（本机全量约 115s、单测试 0.26s），438 通过、3 失败：

1. `tests::external_spill::checkpoint_external_tail_is_spilled_and_restores_identically`，`external_spill.rs:116`：`the oldest 20 entries are spilled — left: 15, right: 20`
2. `tests::external_spill::reconcile_cleans_orphan_cards_and_honors_protection`，`external_spill.rs:268`：`left: 19, right: 20`
3. `tests::fixed_budget_closure::search_coverage_names_the_gap_and_a_continuation_reaches_later_pages`，`fixed_budget_closure.rs:99`：`every cold entry is a manifest row — left: 12, right: 14`

同 run 两个 Linux 分片全绿；三失败集中在 23:02:49–23:03:11，与同模块其余卡片写入型测试（`capture_card_writes…`、`recovery_roots…` 等）并发重叠。

## 根因结论（已由预算注入因果确认）

**checkpoint capture 的卡片写入循环有墙钟预算 `external_checkpoint_io_budget_ms`（默认 2000ms），满载 runner 上预算在循环中途耗尽，剩余计划卡片本次静默留 inline，`external_spilled` 清单因此短于测试要求的第一遍全溢断言。** 这是测试配置缺口，不是生产语义缺陷。

机制全链路：

1. `checkpoint()`（`engine.rs:3847`）三阶段：锁内 `plan_external_spill` → 锁外 `run_external_spill_io` → 锁内登记＋序列化。测试场景（inline 目标 10、外置 30 / inline 目标 0、n=14）首次 capture 无已录卡片，计划写入数＝超额尾全量（20/20/14 张卡）。
2. `run_external_spill_io`（`engine.rs:1412`）每次迭代＝存在性探测读（`store::read_existing_card_bounded`：open＋metadata＋read）＋失败时原子写（`store::write_external_card_async`：create＋write＋flush＋**sync_all/fsync**＋rename）。墙钟从循环前起算，预算检查在 `engine.rs:1462`：`if index > 0 && started.elapsed() >= budget { break; }`——首张写入豁免（进度保证），其后任一迭代越线即停，剩余条目留在 inline。
3. 满载 CI runner（4 vCPU，libtest 默认 4 线程）上多个 spill/fsync 密集测试并发压同一 %TEMP% 盘，单次卡片 IO 被拉到百毫秒级；2s 预算只够 12–19 次迭代——恰好是 CI 观测的 15/20、19/20、12/14。本机低载下 20 次写入远快于 2s，永不触发。
4. 每次迭代的探测读也计墙钟且发生在预算检查之前，放大了满载下的消耗。

排除其他假设：写入失败并非被吞——IO 失败臂是逐条 `continue` 且顺序执行、无有界并发，失败率机制无法解释「同一配置两次失败 shortfall 不同（15 vs 19）」而墙钟竞争可以；`plan_external_spill` 的静态预算（scan 4096 / batch 64 / bytes 8MiB）在本场景均不构成约束；测试隔离（tempdir 每测试独立、无共享全局）无互踩。

## 语义判断与修复选择（真实契约）

选择是**既有语义＋测试钉宽预算**，不是 (a) 拆屏障预算，也不是给生产加新上报。依据：

- checkpoint 屏障的完整性从不依赖 spill 完成：预算截短后留下的条目以**全量元数据内联**进 checkpoint 值（`engine.rs:3889-3895`），清单只点名真实落盘的卡片（fail-closed，宁大不坏引用）。checkpoint 值的 inline/manifest 形状本身就是这次 partial 的类型化事实，恢复完整无损。这是 F2 的成文契约（`SimpleContextConfig::external_checkpoint_io_budget_ms` 字段文档 `engine.rs:227-231`：「When it is spent, the remaining planned cards stay inline and the next capture continues; a slow disk cannot stretch one capture without bound」）；拆掉它是产品语义变更，还会让屏障失去慢盘墙钟界、孤儿化一个公共配置字段，收益不存在（屏障本来就不撒谎）。
- 仓库先例已裁决过同一失败：`cold_bounds.rs:38-42` 记录 run 34999486097（spilled 38/40），处置是把该模块测试配置的预算钉宽到 60s。`batch_required_plan.rs`、`cold_owner_and_required.rs`、`search_continuation.rs`、`spill_claim_integrity.rs` 同此。本任务的三个失败测试所在模块恰是**没钉**的两个。
- 但该注释宣称的「该语义有自己的确定性覆盖」实际缺失：墙钟停止分支（`engine.rs:1462`）此前没有任何确定性测试（已有的是 scan 预算与零预算 recorded 行覆盖）。本次补上。

## 修复内容

- `external_spill.rs::spill_config`、`fixed_budget_closure.rs::carded_history`：`external_checkpoint_io_budget_ms: 60_000`＋机制注释（对齐其余 5 个测试文件的既有处置）。两文件所有引擎构造均展开这两个基配置，钉点完备。
- 新增确定性回归 `an_exhausted_capture_io_budget_stays_inline_and_converges_on_later_captures`（external_spill.rs，序列化预算测试之后）：预算注入 0 钉住墙钟停止的真实语义——首张豁免卡后停止、剩余 29 条留 inline（清单 1＋inline 29＝完整 30 条）、部分分片 checkpoint 可完整恢复（30/30、missing=0）、后续 capture 每次恰续写一张、20 遍内收敛到 20/10、卡片内容寻址无重写。

## 红绿对照与变异验证（真实命令）

| 状态 | 注入 | 结果 |
| --- | --- | --- |
| 修复前 | 两测试助手预算改 0（临时，未提交） | 三测试确定性转红，断言行与 CI 完全一致（left 1/1/1——预算 0 下每 capture 恰写豁免首卡；CI 的 15/19/12 是同一机制的真实墙钟读数），0.16s 内完成，不靠满载碰运气 |
| 修复后 | 预算钉 60_000 | 三测试＋新回归全绿（4 passed，0.38s） |
| 变异恢复 | 生产 `engine.rs:1462` 预算检查注入 `&& false`（临时，已恢复） | 新回归转红：`left: 20, right: 1`（预算停止消失、首遍全溢） |
| 恢复后 | — | 新回归转绿（1 passed，0.28s） |

两条腿都承重：钉宽是三测试转绿的原因（预算 0 即红）；新回归钉住的正是被 CI 放大的那条生产语义（拆预算检查即红）。

## 已执行回归

```
export CARGO_TARGET_DIR=/d/Users/Ye_Luo/APP/cap-agent-target-a
cargo test -p context-simple        # ×3：442 passed / 0 failed（115.07s、112.67s、106.04s）
cargo test -p agent-compose --test kv_production_sequence --test proof_supervision
                                    # 1 passed + 1 passed
cargo fmt -p context-simple -- --check   # clean
cargo clippy -p context-simple --all-targets  # 无告警
```

## 剩余限制

- 生产默认 `external_checkpoint_io_budget_ms: 2_000` 保持不动：真实满载盘上生产 capture 仍会在预算处收敛为「更大的 inline 段」，跨多次 capture 收敛——这是成文语义，不是回归；checkpoint 值没有独立的「预算推迟计数」诊断字段（inline/manifest 形状即事实）。若未来需要显式诊断，属新任务。
- 远端 CI 的最终确认以合入后 Windows 分片为准；本地证据链为「注入转红/钉宽转绿＋3 遍全绿」，不以一次本地绿宣称根因消失。
- `docs/CURRENT.md`、`docs/NEXT_TASKS.md` 未动（集成人维护）。

## 提交

- 测试设施：本分支 `context-simple: pin the capture io budget wide in spill tests and cover the exhausted-budget semantics deterministically`（含两个钉点＋新回归）
- 回执：本文件
