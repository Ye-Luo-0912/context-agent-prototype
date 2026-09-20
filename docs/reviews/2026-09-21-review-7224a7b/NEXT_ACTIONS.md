# 实施任务（V5 审查 7224a7b）

范围：报告第 6 节的可执行修复顺序与第 7 节回归规格。全部切片**零供应商**，在本地控制器测试内完成，
不打开新的付费窗口，不改写 V5 原始失败与冻结证据。

## 顺序 1 — 合同与容量可满足性（F01、F02）

| 编号 | 文件 | 必须满足的退出条件 |
|---|---|---|
| C-1 | `scripts/package_endurance_v5/SPEC.md` | 冻结一个自洽合同：明确是受控协调下的静止切点，规定读取 URI 权限、WAL/sidecar 权限、允许的有界拒绝与恢复义务；删除与 oracle 冲突的 immutable 要求 |
| C-2 | `scripts/package_endurance_v5/oracle.py` | 归档成员数/单成员字节/展开总量/归档字节四个上限成为**唯一定义**（模块常量），供合同文本与预检共同引用 |
| C-3 | `scripts/package_endurance_v5/prepare.py` | 运行前计算最大引用闭包（成员数、展开字节、归档字节）与冻结负载容量，产出 `capacity-plan.json`；容量不可满足时拒绝准备 |
| C-4 | `scripts/package_endurance_v5/preflight.py` | 校验 SPEC 文本声明的边界与 `oracle` 常量一致、容量计划可满足、合同身份（SPEC/oracle/控制器摘要）绑定；任一不成立即 `NOT_READY` |

## 顺序 2 — oracle 与本地反例（F06、F07）

| 编号 | 文件 | 必须满足的退出条件 |
|---|---|---|
| O-1 | `oracle.py::repository_cut` | 使用显式读事务取得一个切点身份；不再用"调用返回后的最新状态"充当唯一正确答案 |
| O-2 | `oracle.py::repository_cut` | 每个 scope 的历史 generation 集合连续且 current 指向最大 generation；重复发布身份、跨 scope manifest、journal 资源闭包、outbox 义务各有明确拒绝 |
| O-3 | `oracle.py::check_archive` | 256/2 MiB/展开总量/归档字节四上限在归档侧强制 |
| O-4 | `oracle.py::check_restored` | 恢复目标使用完整字节映射比较：多余文件、缺失文件、链接均拒绝 |
| O-5 | `probes/probe_v5_oracle.py`（本目录副本） | O1–O4 由红转绿（退出码 0）；新增 outbox 缺失、重复发布身份、跨 scope、journal 闭包四类反例 |

## 顺序 3 — 控制器（F03、F04、F05、F08）

| 编号 | 文件 | 必须满足的退出条件 |
|---|---|---|
| L-1 | `continuous_load.py` | 四个写者、读取/GC worker 与接收者真实重叠：记录区间并可经屏障观察重叠，不以线程对象数量代替覆盖 |
| L-2 | `continuous_load.py` | 历史失败永不删除；另维护按 `candidate_digest` 绑定的验收窗口；替换版本只关闭旧窗口，不重置任务身份与历史证据 |
| L-3 | `continuous_load.py` | 有界 `unresolved_failures` 投影：普通批次推进不能覆盖未解决失败，只有同义务被证实解决才解除 |
| L-4 | `continuous_load.py` | 故障使用 planned/fired/observed/verdict 台账；未观察 `exit 74` 时记 `NOT_TRIGGERED`，并断言失败目录不变性 |
| L-5 | `continuous_load.py` | resume 从权威库恢复各 scope generation 与请求序号；不重用旧 key 或旧发布身份 |
| L-6 | `run.py` | campaign 拥有写者/接收者/负载/模型执行者寿命：模型段结束后不立即终止已冻结负载；授权与动作需求不相容时拒绝开跑 |

## 顺序 4 — 授权、预算与汇总（F09、F10、F11）

| 编号 | 文件 | 必须满足的退出条件 |
|---|---|---|
| R-1 | `runtime_endurance_incremental_runner.py::BudgetLedger` | 说明文字与实际金额预约出自同一来源，子类只改参数不改算法 |
| R-2 | `run.py::V5Ledger` | 策略名与算法一致；跨段主决策/工具受理/尝试次数由账本机械导出并拒绝超额，不信任调用方传入的 rounds |
| R-3 | `runner::_prepare_segment_files`、`run.py` | grant 按冻结 campaign 时长与动作需求生成；开跑前做相容性检查 |
| R-4 | `finalize.py` | 结果从材料派生，缺失原始段/日志坏行时 `INCOMPLETE`；不再硬编码结论与固定路径 |

## 顺序 5 — Runtime/Context 定向优化（报告 5.2、§7"进展分类"）——已关闭（2026-09-21）

| 编号 | 文件 | 必须满足的退出条件 |
|---|---|---|
| L7-1 | `crates/agent-runtime/src/execution/state.rs` | 知识前沿与交付推进分开记账：只读知识更新（含反复读到被外部控制器改写的外部反馈文件）不得清零交付停滞；交付停滞 advisory 有界、只叙述 blocker/未解失败，不阻断执行、不是完成声明 |
| L7-2 | `crates/agent-runtime/src/execution/freshness.rs` | 每轮分类推进类别：已知足迹 mutation／通过的验证／义务解除 = 交付推进；只读证据与重新确认 = 知识更新；未知足迹失效、重复、无进展 = 都不算 |
| L7-3 | `crates/agent-contracts/src/context.rs`、`crates/agent-runtime/src/prompt.rs` | 交付停滞作为独立有界行进入 `TaskProgressView` 与提示 |
| L7-4 | `crates/agent-contracts/src/event.rs`、`crates/agent-runtime/src/actor/tools.rs`、`crates/agent-eval/src/{metrics.rs,bundle.rs,convergence_bench.rs}`、`crates/agent-replay/src/frontier.rs` | 交付债以 additive 字段进入 `ExecutionFrontier` 事件（serde default，旧 journal 读作 0），评测指标/bundle/bench 与重放重建均可见 |
| L7-5 | `execution/tests.rs`、`prompt.rs`、`agent-eval` metrics、`agent-replay` frontier | 回归：心跳不清交付停滞、产物变更/验证通过清交付停滞、未知足迹既不交付也不冒充重复、视图与提示渲染、事件聚合与 trace 重放的交付峰值 |

## 顺序 4 残余 — 工具受理的跨段在飞强制（F10 收口）——已关闭（2026-09-21）

| 编号 | 文件 | 必须满足的退出条件 |
|---|---|---|
| R-5 | `scripts/runtime_endurance_incremental_runner.py` | 新增 `RunnerConfig.tool_budget/tool_budget_baseline`、`_ToolBudgetWatcher`（增量读子进程 journal）、`_wait_for_child(budget=)` 与 `EXIT_TOOL_BUDGET`：campaign 级工具预算在**在飞**阶段执行，基线由材料导出，未配置时行为不变 |
| R-6 | `scripts/package_endurance_v5/run.py` | 冻结 campaign 的 `tool_attempts` 与材料导出的基线交给段配置，并记入 `segment-handoff.json` |

## 回归规格映射（报告第 7 节）

| 规格 | 落点 |
|---|---|
| 合同可满足性 | `preflight` 容量检查 + `scripts/tests/test_package_endurance_v5_contract.py` |
| 有 WAL 的静止/在线源 | `oracle` 读事务与 sidecar 规则 + 合同测试 |
| 早期失败后修复 | `continuous_load` 验收窗口 + 控制器测试 |
| 真实并发 | `continuous_load` 区间证据 + 控制器测试 |
| 断点续跑 | `continuous_load` 权威库恢复 + 控制器测试 |
| 故障门 | `continuous_load` 故障台账 + 控制器测试 |
| 失败持续投影 | `continuous_load` `unresolved_failures` + 控制器测试 |
| 进展分类 | 顺序 5（未实施，记录剩余项） |
| 授权耗尽 | `runner` grant 参数 + `scripts/tests/test_package_endurance_v5_budget.py` |
| 跨段预算 | `V5Ledger` 机械导出 + `test_package_endurance_v5_budget.py` |
| 证据完整性 | `finalize` 派生 + `test_package_endurance_v5_budget.py` |
