# KV 回执 — 生产装配序列验收（agent-compose 测试层）（71f8a586 续审）

工单：`docs/NEXT_TASKS.md` 第九批 §4（C：生产装配序列 KV 验收）。
执行日期：2026-09-17。分支：`kv-production-sequence`（worktree
`cap-kv`，基线 `dfbea60d`）。执行方式：单一切片；不改 provider-openai、
不改生产代码、不改 Cargo.toml/.github、不推翻既有 smoke 与手工矩阵。

## 交付物

- 新增 `crates/agent-compose/tests/kv_production_sequence.rs`，唯一用例
  `production_trajectory_of_one_task_over_the_local_capture_server`。
- 生产代码、`crates/provider-openai`、依赖清单：零改动。无新增 test-only
  seam——全部经既有生产缝（脚本式本地 HTTP 捕获服务器 + 真实
  Compose/Actor/OpenAI Provider/内置工具/Dynamic Context 引擎 + 真实
  `capability.manage`/`fs.*`/checkpoint/restore 命令面）。

## 轨迹与逐步断言

同一个 TaskId、同一个 workspace、同一个捕获服务器（127.0.0.1 随机端口）
上的连续生产轨迹，15 个 HTTP 轮、7 个回合；每轮对比前一轮的最终 HTTP。
模型决策全部由脚本式 SSE 服务器驱动（每轮 `input_tokens` 唯一，账本可
与线上一一对应），装配/选材/装箱/映射全部是生产链路。

| 步骤 | 生产动作 | 断言要点 |
| --- | --- | --- |
| 真实读取 | 回合1：脚本 fs.read `notes/alpha.txt` → fs.write `evidence.txt` → final | **磁盘逐字节**断言 evidence.txt == `EVIDENCE-V1-ARCHIVE-BYTES`（read_only 审批证明不了写入，本测试用 permissive 审批＋磁盘真值）；R2 起读取结果正文（ALPHA-REV1 sentinel）真实在线 |
| 新证据进入 | 回合2：fs.read `notes/beta.txt` | 回合1 提交后 R4 首次声明 B1（紧随 B0 的 SELECTED WORKING CONTEXT 块）并携带回合1 的读取观察；R5 携带 BETA sentinel；R3→R4 首差异在稳定策略之后 |
| 焦点改变 | `steer_active_task`（同任务内改向，接受回执 Applied） | R6 携带新指令文本「T3 archive focus now」；B0 不动、首差异在 B0 之后 |
| 文件版本改变 | fs.write 覆写 alpha → fs.read 回读 | 磁盘 alpha == REV2 字节；R9 携带 REV2 正文；回合4 提交后 R10 的**声明证据项**载入 REV2 版本体 |
| 工具撤销 | 模型自身调 `capability.manage load/unload fs.mkdir`（生产目录控制面） | fs.mkdir 仅在 R11（load 后下一轮）出现在 wire 工具表，其余 14 轮不在；基线表每轮都在场；同名工具跨轮 schema 逐字节相同 |
| checkpoint/恢复 | `instance.checkpoint()` → shutdown → **真实重组** → restore → `continue_active_task` | 任务身份（TaskId）不变 → routing key 跨恢复不变；B0 策略项跨进程逐字节相同；声明证据区（SELECTED WORKING CONTEXT）存活；工具目录是组合层级，重组后 fs.mkdir 回到 catalog-cold（如实断言） |
| 失败后费用结算 | `response.failed`（不可重试码）+ usage 1015/44 | `Failure` 事件；账本 15 行与 15 轮**一一对应、按序、各恰一次**，全部 `Observed`、计数与脚本完全一致、无 Unknown 行、无未脚本的多余轮 |
| 正文完整性 | 每步 | 必需正文（读取结果、写入回执、交换回执、恢复后工件正文）在对应轮真实在场；R14 携带**磁盘已验证**的 evidence.txt 字节经真实 fs.read 重新进入请求正文 |

跨全部 15 轮不变量：`prompt_cache_key` 恒等且等于
`routing.key_for(task,"main")`（`rc2-` 摘要）；`prompt_cache_options.mode
= explicit`；B0 项逐字节恒定且钉在同一 wire 位置。

## 真实命令与结果（Windows，共享 `CARGO_TARGET_DIR=cap-agent-target-a`）

```text
cargo test -p agent-compose --test kv_production_sequence
  → ok. 1 passed; 0 failed（连续 4 次通过，含 3 连发稳定性验证）
  用例本体 9.6–9.9s / 含增量编译 wall ≈10–16s
cargo test -p agent-compose --test cache_wire_flow
  → ok. 1 passed（4.08s）——既有 smoke 未退化
cargo test -p agent-compose --test cache_routing_wire_acceptance
  → ok. 3 passed（4.29s）
cargo test -p provider-openai task_sequence
  → ok. 6 passed; 163 filtered（0.02s）——手工八步矩阵未破坏
cargo fmt -- --check → clean
cargo clippy -p agent-compose --all-targets → 0 warnings
python scripts/doc_consistency.py → OK
```

运行期事实（`--nocapture` 记录）：R1–R3 仅 B0（回合内证据未入册）；
R4 起 B0+B1；`context_manage` 随 NeedEvidence 租赁在 R5/R9/R11/R12/R14
出现、其余轮不在（见「发现」）。

## 四级状态

- **LOCAL_WIRE = PASS**（上表全部断言在本机真实运行通过，4 次）
- **ENDPOINT_ACCEPTED = NOT_RUN**（无授权凭据，未运行任何 paid/ignored 测试）
- **SERVER_HIT = NOT_RUN**
- **NET_TASK_COST = NOT_RUN**

## 发现（如实记录，未改生产语义）

1. **`context.manage` 在相邻轮之间进出的表面积波动**：NeedEvidence
   租赁按决策边界 reconcile（item 24 语义），读结果落地的那一轮
   surface 它、下一轮又撤下。每次进出都改变 `tools`，从而（依 provider
   矩阵已证明的 boundary 绑定工具面）使供应商侧前缀复用边界失效。机制
   正确、非缺陷；但该进出节奏对真实端点复用是成本相关观察，建议 T8 的
   供应商实验单独核对它，不在本切片调整。
2. 回合内的早期轮（证据入册前）只声明 B0——与既有 smoke 的 N05 结论
   一致，在生产装配下复现。
3. 工具目录生命周期是组合层状态，不进 RuntimeCheckpoint：恢复后
   fs.mkdir 回到 catalog-cold。这是当前架构事实，回执记录，不改。

## 测试过程缺陷（非生产缺陷）

首版测试在两个 runtime 会话间收集账本时死锁：`RuntimeHandle` 持有
broadcast sender，collector 等待通道关闭永远等不到。属测试自身缺陷，
已修复（先 drop handle 再 join，并对 join 加 30s 上界）。

## 剩余限制

- 工件分页读取（F1/F2 取回）未纳入本轨迹——工单为「可含」；现有
  `artifact.read` 常载表面与 broker 裁剪维度由 E1 跨层测试与 tool-runtime
  分页测试承载。若要把分页取回接进 KV 序列，需在轨迹中真实制造溢出工件。
- 取消支路在本测试未驱动（失败支路已驱动）：取消账目由既有
  `cancel_usage_settlement.rs`（compose 层）与 provider 层 cancel-late-usage
  测试覆盖，未重复建设。
- 本轨迹 usage 全部为已知计数（脚本必报 usage）；Unknown 行通道由
  `cancel_usage_settlement.rs` 的 (0,0,Unknown) 断言覆盖。
- 恢复后回合内的 RESTORED TURN BODIES 路径未触发（checkpoint 取在回合
  间空闲点，正文经引擎证据区而非恢复块回流）；该路径的 wire 行为由
  provider 手工矩阵 R6 维度钉住。
- 端点接受、真实命中、净费用：NOT_RUN（见上）；不以本地线装稳定性
  替代真实端点证据。
