# 实施回执：V5 审查 7224a7b 的顺序 1–5

本回执记录按 [REVIEW.md](REVIEW.md) 第 6 节顺序 1–5 与第 7 节回归规格所做的本地实施。
**全部切片零供应商**：没有付费调用、没有打开新的实验窗口、没有改写 V5 原始失败与冻结证据。
顺序 1–4 为控制器/裁判/授权侧（第 1–5 节），顺序 5 为 Runtime 交付推进口径（第 6、8 节）。

- 固定基线：`7224a7baa8910881b9efaa77f14013781137ff49`（分支 `codex/runtime-endurance-full-plan`）
- 本次改动全部在工作树，未提交、未推送；远端 CI 未触发（该基线本身无 workflow run）
- 命令环境：Windows + CPython 3.13.14 + cargo/rustc 1.97.1（顺序 5 起）；未运行 .NET 构建

## 1. 冻结合同（F01、F02）

**F01 结论：合同与裁判统一为受控静止切点下的 `mode=ro` 读取。**

- `SPEC.md` 删除"immutable read-only URI"要求，明确：控制器在 `backup_live`/`restore_live` 调用期间
  **静止**写者与 GC，并在调用前后各取一次源指纹；读取使用 percent-encoded 只读 URI（`?mode=ro`），
  该模式纳入已提交 WAL 且拒绝一切写入；`immutable=1` **不得使用**（SQLite 将其定义为"文件不会变化"的承诺，
  对在线源不成立，并会静默隐藏已提交 WAL）。
- 切点身份：一次显式读事务覆盖全部 authority 读取，引用字节在同一事务窗口内读取并校验哈希；
  窗口内发生变化产生**有界拒绝**（`ValueError`/`OSError`），不是部分成功。
- 源中不属于引用闭包的文件既不要求也不拒绝；`root` 不被备份修改。
- `preflight` 新增合同一致性检查：解析 SPEC 文本声明的边界并与 `oracle` 常量逐项比对，
  不一致即 `NOT_READY`；`prepare` 产出 `contract-identity.json` 绑定 SPEC/裁判/控制器哈希。

**F02 结论：资源上限按冻结负载正式调整，并在开窗前计算。**

审计自身给出 463 成员的下限（357 objects + 91 manifests + 13 pointers + descriptor + outbox），
已超过原 256 成员上限。本批只调整**成员数**上限，字节类上限保持不变，依据是冻结负载的最大引用闭包：

| 项 | 冻结值 | 旧值 | 计算依据 |
|---|---|---|---|
| 成员数 | 1024 | 256 | 计算闭包 626（480 objects + 120 manifests + 16 pointers + 5 journal + 4 outbox + descriptor）；审计自身需 463 |
| 单成员字节 | 2 MiB | 2 MiB | 最大成员是 descriptor：4096 receipts × 384 B + 开销 = 1,581,568 B |
| 展开总量 | 8 MiB | 8 MiB | 计算 2,067,104 B |
| 归档字节 | 9 MiB | 9 MiB | 计算 2,109,470 B |

- 冻结负载写入 `workload.py`（唯一来源，`prepare`/`preflight`/控制器共用）：
  `INSTALL_BUDGET_PER_WINDOW=1024`、`MAX_WINDOWS=4`、总安装预算 4096、4 写者、每 4 次安装一次归档。
- `prepare.py` 开窗前计算并落盘 `capacity-plan.json`，不可满足即拒绝准备；
  `preflight.py` 重新计算并与记录比对（漂移即 `NOT_READY`）。
- 模型侧不再需要为满足成员上限而裁剪历史 receipt：合同明确"不得加宽上限、候选不得为满足上限而封顶历史 receipt"。

新合同身份（本次实现，与 V5 原始失败证据分开）：

```
SPEC.md                286ad4579f45b29d2fe162a47bd9efd64bcae0d15a257a6367c45e870bf0bc0e
oracle.py              711bbb8f6d533f948e780f40b2afcb442edb6f9090df6d11b0791f4802ed3ef0
workload.py            5b5ed5491086f9ac6d2b9f63b7727be92a89961eed5e909c1ddbf116f2d23a7d
continuous_load.py     f9ba11924353ab4ea60fae48f21576f78c50bf7764e2559a893d313a03794910
campaign_accounting.py 46eb4745cfcb0b431db69e2c35d4e92d41f4b4aeaa39c91d27577bc77a7b392b
runner_grants.py       07ae8b5bd14493686b9949b5d782987b96524998ba2bf4db310885169d8d7d55
prepare.py             e2231783841fbf47ee5be88cec66f584c18201ce167e227f13105326226a78f1
preflight.py           37616c7952a85c79ee057ba87a7dc439e13f17ea4d2e3b4e6698e8151f6a70b5
finalize.py            8d70fba63499856c242db12cac31e370c3da5226e12140d51c244d0527a05d92
run.py                 511d7f6fb723f8013160a8c52a654c1fe08db3f3413ef3c5751bca08431ca6c4
runtime_endurance_incremental_runner.py e65f52a742880bb9b6fa07208b5b5eadfda4f37f15d311d4af6927226c6165ec
```

## 2. 裁判不变量（F06、F07）

`oracle.py` 现在强制（此前 O1–O4 全部漏检）：

| 反例 | 不变量 | 修复后 |
|---|---|---|
| O1 | 每个 scope 的历史 generation 连续（1..N） | 拒绝：`non-contiguous historical generations` |
| O2 | `current` 指向最大 generation，且与 SQL/磁盘指针一致 | 拒绝：`current generation 1 is not the maximal generation 3` |
| O3 | 恢复目标完整字节映射（多余文件/链接/缺失） | 拒绝：`extra file in restored destination` |
| O4 | 归档成员上限（冻结值 1024） | 拒绝：`archive member bound: 1027` |
| O5 | 单成员字节上限 | 拒绝：`member byte bound` |
| O6 | 展开总量上限 | 拒绝：`uncompressed total bound: 10486891` |
| O7 | outbox 义务必须被归档表示 | 拒绝：`outbox member missing or mismatched` |
| O8 | 发布身份不可重复 | 拒绝：`duplicate outbox identity` |
| O9 | manifest 只服务自己写入时的 scope | 拒绝：`cross-scope manifest` |
| O10 | journal 资源闭包 | 拒绝：`journal new_manifest outside the cut` |
| P1 | 正对照：规范切点与规范归档仍被接受 | 接受 |

F07 另修：`repository_cut` 使用显式读事务（`BEGIN` + 只读连接），引用字节在同一窗口读取；
新增 `source_fingerprint`，控制器在调用前后各取一次并在不一致时**拒绝该批次**（不计入候选失败）；
SQLite `-shm` 与空 `-wal` 不进入指纹（只读读者可合法附着），非空 `-wal` 参与哈希。

审计原探针（本目录 `probes/probe_v5_oracle.py`，未改动）复跑结果：
**O1/O2/O3 由红转绿**；O4 的"拒绝 263 成员"期望按 F02 被正式调整取代（现按冻结上限 1024 验证），
因此该断言仍报 accepted——两条结论分别记录，不合并成"O1–O4 全绿"。

```
python -B docs/reviews/2026-09-21-review-7224a7b/probes/probe_v5_oracle.py --repo .  → exit 1（仅 O4 期望被取代）
python -B scripts/package_endurance_v5/probe_oracle_invariants.py --out …          → exit 0，leaked 0
```

## 3. 控制器（F03、F04、F05、F08）

- **真实并发（F03）**：4 个独立写者线程以 barrier 同步轮次，各自驱动独立候选子进程；
  独立 reader/GC 执行者；独立 verify 执行者在新一轮写入继续时做 restore + 裁判校验。
  区间证据落盘 `intervals.json`，收据给出可测重叠：
  写者×写者、verify×写者、GC×写者。`writer_writer.pairs == 0` 直接判 `FAILED`。
- **验收窗口（F04）**：按 `candidate_digest` 绑定窗口；历史失败永不删除，替换版本关闭旧窗口、
  开新窗口，不重置任务身份与历史证据。窗口判定 = 时长 + 通过率 + 故障覆盖，
  且仍有未决失败时不得 `PASS`。
- **未决失败投影（F08）**：有界 `unresolved_failures`（上限 16，丢弃计数入账），
  只有**同义务（同 scope/operation）的成功**才解除；普通批次推进不覆盖。
  `runtime-feedback/latest.json` 始终携带该列表。
- **故障门（F03 要求）**：planned/fired/observed/verdict 台账。未观察到 `exit 74` 时记
  `NOT_TRIGGERED`（同时把"边界未按合同触发"记为未决失败，不静默放过）；观察到 74 后
  必须重试成功并记录前后目录清单，否则 `FIRED_NOT_RECOVERED`。
- **断点续跑（F05）**：各 scope generation 与最大已用序号从**权威库**读取（不信任内存），
  续跑以新 key 继续，永不把旧身份当新增负载；`resume-evidence.json` 记录起始槽位与权威快照。
- **寿命（F05）**：`run.py` 启动负载后不再因模型段结束而终止它；模型段结束后在冻结界内
  等待纯本地负载自然结束，`segment-handoff.json` 分别记录段结束与负载状态。
- 另修两处真实缺陷（在本地端到端干跑中暴露）：
  (a) 发布身份取自并发写者的非原子 `max()`，可能取到仍在安装中的槽位 → 改为只由**已提交**安装入队；
  (b) 控制器接收者未实现应用文档化的 `GET receipt?tenant=&environment=&key=` 身份查询
      （原实现返回审计列表），合法重试无法对账 → 按协议实现（200 原始正文 / 404 未知身份），
      并保留"首个 ACK 丢失"的既定故障注入。

## 4. 授权、预算与汇总（F09、F10、F11）

- **F10**：`BudgetLedger` 的说明文字与金额算法现在出自同一处
  （`reserve_input_tokens_per_byte` + `reserve_input_padding_tokens` → `estimate_reserve_usd` 与
  `input_estimate_text`），`V5Ledger` 只改参数：策略名 `wire_bytes_plus_8192_and_full_output` 与实际
  `(body_len + 8192)` 算法一致。默认账本行为与历史一致（`body/4 + 256`）。
- **跨段机械受限**：新增 `campaign_accounting.py`，主决策/工具受理/供应商尝试数由账本与各段
  `summary.json` 导出（readable 段计入，坏段显式列入 `incomplete_segments`）；
  `run.py` 以导出余量夹取 `rounds`，余量为 0 时拒绝开跑。调用方传入的 rounds 不再被信任。
- **F09**：新增 `runner_grants.py`，授权按冻结 campaign 生成（Python 执行数 = 决策数×8+64，
  到期为 campaign deadline），并由 `preflight` 与 `run.py` 在开窗前做相容性检查；
  `RunnerConfig.grants` 让冻结 campaign 覆盖共享 runner 的短测试默认值（48 次 / 30 分钟）。
- **F11**：`finalize.py` 重写为从材料派生：段列表、控制器窗口、候选摘要、候选自测日志、
  保护文件状态、源 summary 全部读回；缺材料 `INCOMPLETE` 并列出缺失项；候选测试日志按发现记录，
  缺失即 `NOT_RECORDED`（不再硬编码 r3 路径与固定结论）；`MANIFEST.json` 覆盖输出目录全部文件。
  新增门：控制器通过但**无模型段材料**时判 `INCOMPLETE_NO_MODEL_SEGMENT`，仅控制器阶段不能派生成 ACCEPTED。

## 5. 真实执行结果（零供应商）

```
python -B scripts/package_endurance_v5/prepare.py <tmp campaign>
  → protected_files 496、capacity_satisfiable true（与 V5 原始 496 保护文件一致）

python -B scripts/package_endurance_v5/preflight.py <tmp campaign> --out PREFLIGHT.json
  → status READY
     contract bounds = {members 1024, member_bytes 2097152, uncompressed 8388608, archive 9437184}
     capacity plan  = members 626 / descriptor 1581568 B / uncompressed 2067104 B / archive 2109470 B
     authorization  = compatible，需 2144 次 Python 执行（旧默认 48）
     oracle calibration = 正对照接受 + object/pointer/receipt 三处变异全部拒绝

python -B scripts/package_endurance_v5/continuous_load.py <campaign> --seconds 10（工作区无候选模块）
  → INCOMPLETE；写者×写者重叠 6 对（最长 1.61s）、GC×写者 12 对（2.03s）
     未决失败：backup(ModuleNotFoundError)、crash_boundary(期望 exit 74，实见 0)
     故障台账 NOT_TRIGGERED；未产生任何归档 → 不判 PASS

python -B - <<以测试参考实现驱动同一真实 campaign 的端到端干跑>>
  → PASS：5 次归档 / 5 次 restore+裁判通过 / 0 拒绝
     重叠：写者×写者 10 对 1.37s、verify×写者 8 对 1.64s、GC×写者 24 对 1.94s
     故障覆盖 1/1 OBSERVED；窗口 verdict PASS、无未决失败
     说明：该候选为 test fixture（`scripts/tests/_v5_reference.py`），不是模型产物，不构成模型证据

python -B scripts/package_endurance_v5/finalize.py <campaign> --output <dir>
  → INCOMPLETE_NO_MODEL_SEGMENT（派生正确：该干跑阶段没有模型段材料）
```

回归（本机实测）：

```
python -B -m unittest discover -s scripts/tests                        → Ran 119, OK
python -B -m unittest discover -s scripts/package_endurance_v3/tests   → Ran 40, OK
python -B -m unittest discover -s scripts/package_endurance_v4/tests   → Ran 31, OK
python -B scripts/package_endurance_v5/probe_oracle_invariants.py      → exit 0, leaked 0
```

新增回归文件：`scripts/tests/test_package_endurance_v5_{contract,oracle,controller,budget}.py`
（43 项，`scripts/tests` 由 76 项增至 119 项）＋按文件路径加载被测模块的 `_v5_modules.py`／测试夹具 `_v5_reference.py`。

## 6. 顺序 5（Runtime/Context 交付推进口径，F11 之外的 5.2 与 §7"进展分类"）

报告 5.2 的链路是：提示提到 `runtime-feedback/latest.json` → 该路径成为 rooted target → 每批次反馈里的
`batch`/`elapsed` 变化 → `fs.read` 得到新证据摘要 → `freshness.rs` 判为 `EvidenceAdvanced` →
`update_convergence` 清零前沿停滞计数。结果是"又读到一份新的监控快照"可以无限推后任务级停滞。

**修复（口径分离，不新增权限、不伪造验收）**：

| 位置 | 改动 |
|---|---|
| `execution/state.rs` | `ConvergenceState` 增加 `actions_since_delivery_advance`；新增 `FrontierProgress{None,Knowledge,Delivery}`；`update_convergence` 除知识前沿外另记交付债；新增 `delivery_warning()`（阈值 `DELIVERY_ADVISORY_THRESHOLD`，与前沿同值但口径不同）；`FrontierObservation` 上报交付债 |
| `execution/freshness.rs` | 每轮分类推进类别：已知足迹的 mutation、**通过的**验证、义务解除 → `Delivery`；只读证据/重新确认 → `Knowledge`；未知足迹失效、重复、无进展 → `None` |
| `agent-contracts/context.rs` | `TaskProgressView` 增加 `delivery_warning`（serde default + skip-if-none），并纳入 `is_empty` |
| `agent-runtime/prompt.rs` | 该行按 `PRIO_DELIVERY`（与 stall 同组最高优先）渲染，含当前 blocker 与未解失败数 |
| 回归 | `execution/tests.rs` 4 项 ＋ `prompt.rs` 1 项：外部反馈心跳不清交付停滞、产物变更/验证通过清交付停滞、未知足迹既不交付也不冒充重复、视图投影携带该信号、提示渲染与硬上限 |

语义边界（如实）：只读知识更新仍然推进**知识**前沿（`frontier_warning` 行为不变），只是不能清除**交付**停滞；
交付停滞仍是 prompt advisory，不阻断执行、不是完成声明、也不是任何权限来源。`FrontierDelta` 词汇保持不变，
`ExecutionFrontier` 事件只做 additive 扩展（新增 `actions_since_delivery_advance`，serde default）并在
`agent-eval`（`frontier_delivery_no_advance_peak` 指标 + bundle 键 + bench 行）与 `agent-replay`
（`FrontierRebuild.delivery_no_advance_peak`，从真实 trace 重放）落地。

## 7. 边界与剩余项（如实）

1. **交付债已进 wire（同日补）**：`ExecutionFrontier` 事件新增 `actions_since_delivery_advance`（serde default，
   向后兼容），`actor/tools.rs` 逐字段复制观察值；`agent-eval` 增加 `frontier_delivery_no_advance_peak`
   指标与 bundle JSON 键、bench 行加入 `delivery_no_advance`；`agent-replay` 的 `FrontierRebuild` 增加
   `delivery_no_advance_peak`，从真实 trace 重放得出。旧 journal（无该字段）读作 0，不改变任何执行决定。
2. **工具预算按轮询粒度执行**：`tool_attempts` 现在由 runner 在**在飞**阶段执行（`_ToolBudgetWatcher`，
   增量读子进程 journal，上限跨段由材料导出），触发后以既有有界停机路径停止子进程并给出 `EXIT_TOOL_BUDGET=21`；
   计数口径是 `tool_finished`（与收据既有 `tool_calls` 同一词汇），检查粒度约 100 ms，
   因此超支最多再发生一个轮询窗口内的调用。
3. **写速率受容量约束**：descriptor 成员承载全部 receipt，因此安装预算被冻结为 4096 并跨 4 个窗口分摊；
   120 分钟窗口内的写压力因此低于"不设上限"的情形，读/GC/校验负载用于填满窗口。
4. **审计 O4 期望按 F02 被取代**：这不是"探针全绿"，见第 2 节的两条分开结论。
5. **未执行**：真实供应商重跑、.NET 构建、真实候选模型运行、远端 CI（该基线无 workflow run）、GUI。
6. **原始失败保留**：`NOT_ACCEPTED_MODEL_BUDGET_EXHAUSTED`、V5 冻结证据与 `docs/experiments/package-endurance-v5/`
   一行未改。
7. **本机 Windows 低完整性 wrap 路径不可用（已按 C0 方法定位为宿主环境条件，非本批改动）**：
   - 现象：`agent-process::containment::wrap_job_kills_the_child_and_its_immediate_descendant`
     连续 3/3 复现失败；`agent-process --test host::cancel_without_peer_ack_still_kills_after_the_bound`
     连续 2/2 失败；`agent-eval::platform_closure_m13` 仅两个"受限沙箱档位"行解析不出（其余档位正常）；
     `agent-eval::long_live` 的后代存活探测首跑失败、单独复跑通过（CURRENT.md 已记录的满载抖动类）。
   - 本地探针（全部零供应商）：
     1. `mock_host.exe --serve` + `MOCK_MARKER=1` 直接启动 → 存活正常（夹具本身可用）；
     2. 工具沙箱内/外结果一致（`dangerouslyDisableSandbox` 复跑仍失败）→ 非本会话工具沙箱所致；
     3. 本会话不在 Job 对象内（`IsProcessInJob` = False）→ 排除"外层 Job 嵌套"；
     4. 手动运行 wrap（`<binary> __FOCUS_AGENT_INTEGRITY_WRAP_V1__ <target> …`）：
        缺程序 → 打印 `integrity wrap: missing program`（exit 2）；程序不存在 → 打印
        `integrity wrap: spawn: program not found`（exit 1）；而目标为 `cmd.exe /c echo`、
        `sandbox_probe alloc 1`、`sandbox_probe fsize <file> 128`、`mock_host --serve` 时
        **一律 exit 1、stdout/stderr 全空，且 `fsize` 的目标文件从未创建** ⇒ 目标进程从未执行；
        同一 `sandbox_probe fsize` 不加 wrap 时正常写出 128 字节文件 ⇒ 目标程序与路径无误。
   - 结论：`contained_spawn::spawn_contained` 的 assign/resume 失败分支（会打印 step/detail）**未被触发**，
     即 wrap 认为分配与恢复均成功，但低完整性子进程没有任何可观察的执行结果；这是宿主级条件
     （疑似宿主安全软件/策略对"低完整性 + Job"子进程的干预），需在干净主机或 CI 复核。
   - 与本批的关系：`crates/agent-process/**` 本批**一行未改**，且它只依赖 `agent-contracts`（本批仅做
     `#[serde(default)]` 的 additive 字段，不在 spawn 路径上）；`mock_host` 在改动后仍能独立运行。
     排除上述项后其余套件全绿（见第 8 节）。

## 7.1 改动文件

修改：`scripts/package_endurance_v5/{SPEC.md,oracle.py,prepare.py,preflight.py,continuous_load.py,run.py,finalize.py}`、
`scripts/runtime_endurance_incremental_runner.py`（`BudgetLedger` 单源预约策略、`RunnerConfig.grants`、
`RunnerConfig.tool_budget/baseline`、`_ToolBudgetWatcher`、`_wait_for_child(budget=)`、`_child_event_rows`、`EXIT_TOOL_BUDGET`）、
`crates/agent-runtime/src/execution/{state.rs,freshness.rs,tests.rs}`、`crates/agent-runtime/src/prompt.rs`、
`crates/agent-runtime/src/actor/tools.rs`（交付债进事件）、`crates/agent-contracts/src/{context.rs,event.rs}`、
`crates/agent-eval/src/{metrics.rs,bundle.rs,convergence_bench.rs}`、`crates/agent-replay/src/frontier.rs`。
新增：`scripts/package_endurance_v5/{workload.py,campaign_accounting.py,runner_grants.py,probe_oracle_invariants.py}`、
`scripts/tests/{_v5_modules.py,_v5_reference.py,test_runner_tool_budget.py,test_package_endurance_v5_contract.py,test_package_endurance_v5_oracle.py,test_package_endurance_v5_controller.py,test_package_endurance_v5_budget.py}`、
本目录（审查归档）。
未改动：`scripts/package_endurance_v5/invoke.py`（候选隔离调用口未变）；
`crates/agent-runtime/src/execution/{classify.rs,body_cache.rs,memo.rs,needs.rs,snapshot.rs}` 与本轮无关，未动。

## 8. 顺序 5 的实测（本机）

```
cargo test -p agent-runtime --lib                          → 451 passed / 0 failed（新增 5 项：446→451）
cargo test -p agent-runtime --tests                        → actor 101、approval 4、host 32、instance 31、
                                                              recall 3、turn 165(1 ignored) 全绿
cargo test -p agent-contracts                              → 198 passed
cargo test -p agent-conformance                            → 35 passed（16 lib + 11 adapter + 5 builtin + 3 boundaries）
cargo test -p agent-compose --lib / --test kv_cache_walk    → 43 passed / 5 passed(5 ignored)
cargo test -p agent-tui -p agent-host                      → 108 passed（tui 106+2）/ 35 passed
                                                              （host 11+3+10+6+3+2），EXIT=0
cargo test -p agent-replay                                 → 63 passed（含新增 delivery 峰值重放回归）
cargo test -p agent-eval --bin agent-eval                  → 224 passed / 2 failed / 1 ignored
   （2 项为本机夹具/握手类失败，与本批改动无关且所在 crate 未改，见第 7 节第 7 条；
     排除后 196 passed / 0 failed / 1 ignored；新增 `delivery_debt_stays_visible_while_knowledge_keeps_advancing` 通过）
cargo fmt --all -- --check                                 → clean
cargo clippy -p agent-runtime -p agent-contracts -p agent-eval -p agent-replay --all-targets -- -D warnings → 通过
python -B -m unittest discover -s scripts/tests            → Ran 124, OK（新增 4 项 runner 工具预算 + 1 项 V5 接线）
```

环境事实（不影响结论，供复现参考）：本机 cargo 偶发 `os error 5（拒绝访问）` 写 `target/` 指纹或清理增量目录，
重试即通过（本轮 `cargo check/test` 各出现过 1–2 次，重试后全部成功）；直接写入与删除同一路径在 Python 侧正常。
本批另修一处**由本轮引入的缺陷**：重写 `scripts/package_endurance_v5/run.py` 时漏掉 `import os` 与
`next_load_name`/`latest_repository` 定义，且此前没有测试覆盖到该路径（`continuous_load.py` 干跑绕过了它）；
新增 `FrozenToolBudgetWiring` 测试后暴露并已修复，`run.py` 现在有直接回归覆盖。

