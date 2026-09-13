# COST-5 有界真实运行回执（2026-09-13）：PREPARED → EXECUTED（有界）

本轮回执关闭 COST-5 的「真实执行」缺口：按既有固定三任务 harness（单源引用 `2026-09-11-flash-workflow/run.py`，官方 DeepSeek deepseek-flash、responses 协议、provider_default 缓存、reasoning off）执行**同起点双臂**配对——default 臂（compose 默认预算）vs budgeted 臂（COST-8 预算旋钮：`MAINTENANCE_MAX_CALLS_PER_MAINTAIN=2`/`MAINTENANCE_MAX_TOKENS_PER_MAINTAIN=20000`/`MAINTENANCE_COMPACT_FAILURE_BACKOFF=4`）。二进制为本轮源码重建的 `target/debug/agent-tui.exe`；记账提取器已先对 09-11 证据逐 token 交叉验证。

## 运行事实

| 臂 | 轮数 | 输入(下界) | 输出(下界) | 缓存读 | write | miss | 压缩行 | 账目完整 |
|---|---|---|---|---|---|---|---|---|
| default | 21 | 105,081 | 4,183 | 69,632 | 未报告 | 未报告 | 0 | **True**（无 unknown 行） |
| budgeted | 20 | 93,715 | 3,461 | 70,272 | 未报告 | 未报告 | 0 | **True**（无 unknown 行） |

**质量门**：两臂 × 三任务产物验收全部 **PASS**（a 跨文件重构＋九域 pytest＋委托探针；b 九域审阅 validation.json；c 长输出＋取消/恢复 summary.json 的 final_marker 与 9 域计数）；取消→强制树停→`--restore` 恢复→续读编排真实执行；`unexpected_changes` 空。

证据归档：[`cost5-run-20260913/`](.)（`cost5_paired.json` 汇总＋两臂 evidence 目录：manifest/events/result/verification）。

## 诚实结论（按任务书判定门）

1. **同质量**：达成——两臂全部产物验收通过、无越权改动。
2. **账目完整性**：达成——两臂 `bill_lower_bound=False`，全程无 unknown 降级行（优于 09-11 基线的 1 次取消缺测）；COST-6 桶语义在真实 wire 上按语义呈现：缓存读有报告、write/miss 该端点未报告（如实缺席，非零充数）。
3. **降本声明：不成立、不声明。** 两臂**压缩事件均为 0**——固定三任务不触发 Rolling 折叠，COST-8 预算/退避杠杆在本任务集上**未参与**；Token 差异（in −10.8% / out −17.3%）轮数不同（21 vs 20）、缓存方差主导，不可归因于任何优化。按任务书「仅 Token 改善就只声明 Token 改善；不能从静态字段存在推断保证」——本运行连 Token 归因都不声明。
4. **下一窗口范围**：降本判定需要**压缩密集**的固定长任务（跨 episode 多次折叠），使 default 与 budgeted 臂的维护调用真实分叉后再配对；本运行确立的是「真实链路端到端通路＋双臂协议＋记账表」。

## 副作用与共享树注意（如实记录）

- 委托原 main() 流程会覆写 `docs/reviews/2026-09-11-flash-workflow/latest.json` 指针（现指向 budgeted 臂运行目录）。该文件从未入库；09-11 运行的证据早已归档于其 `evidence/` 目录，运行数据目录仍在 `target/flash-workflow/` 下。此覆写属脚本流程的已知副作用，记录在此。
- API 消耗：两臂合计约 20 万输入 / 7.6 千输出 tokens（其中约 14 万为缓存读计价），固定协议上限内。

## 共享树注意（如实记录）

default 臂运行窗口内 `sources_unchanged=False`——共享工作树在运行期间被并行会话编辑（manifest 记录的起点哈希与结束时不一致）；agent 在隔离种子工作区内工作且验收检查工作区而非 REPO，产物质量门不受影响。budgeted 臂 `sources_unchanged=True`。

## 状态

**COST-5：EXECUTED（有界、三任务、双臂）——同质量与账目完整性通过；降本不声明（杠杆未参与）。** 压缩密集任务的降本配对沿本协议在下一窗口执行；不重开冻结 M15，不建评测平台。
