# RUN 2026-09-19 — 有界付费短轨迹：失败→同任务恢复→纠正→继续（BR5–BR7 实战验收）

- serving：`deepseek-flash @ api.deepseek.com`（Responses 兼容协议，经本地转发 relay；凭据不落盘）
- runtime 二进制：`97bfb41b…`（从 batch-12 后的 `ba0951c1` 本地构建）；campaign 身份 `identity_ok`
- campaign：`target/runtime-endurance-v1/short-traj-final/`（排他创建；硬额度 $1.00，逐段 $0.40→$0.70→$1.00 递进上界，campaign 级账本跨段继承）
- 任务：与 2026-09-19 FULL-PLAN 相同的 incremental-platform v1→v2 升级（同种子、同起点、同 oracle）

## 轨迹与分层结果

| 段 | 注入 | 实际行为 | 轮次/工具 | 终结 | child exit | runner exit_code | 花费（估计） |
|---|---|---|---|---|---|---|---|
| s1-fail | `--max-output-tokens 384` | 6 轮后 `turn_failed(class=model_output_limit, retryable=false)`，任务保持未完成 | 6 / 18 | turn_failed | 1 | 10 (child_nonzero) | $0.0064 |
| s2-recover | `--restore=latest --continue` | 同任务冷恢复成功（journal 含 `runtime_restored`），继续实现 14 轮 | 14 / 33 | turn_completed | 0 | 0 | 累计 $0.0669 |
| s3-correct | 纠正指令 R-NEW-1（outbox 幂等/旧 token 防护的应用测试＋RESULT.json 清单） | 指令确认送达（prompt 含 2 处 R-NEW-1）；继续实现 16 轮后 `turn_completed` | 16 / 33 | turn_completed | 0 | 0 | 累计 $0.1369 |

全程：36 次请求、unknown=0、`protected_unchanged=true`、`task_completed=false`／`awaiting_operator_review`（headless 诚实语义，不自动宣称完成）。

**机制验收（全部通过）**：输出上限失败注入 → 同任务跨进程恢复 → 纠正指令送达 → 预算账本跨段继承与递进上界 → 保护文件零改动 → 分层退出码与终态回执。

**应用状态（如实，不宣称完成）**：应用真实增长（新增 `validation.py`/`fingerprint.py`/`manifest.py`/`reference.py`/`schema.py`/`workers.py`，store 升级为原子 GC 的内容寻址存储且保留 v1 API）；公共测试 2/2 与 oracle 全程保持通过。但 R-NEW-1 的应用测试与 RESULT.json 未交付，v2 任务整体未完成——36 轮的有界轨迹本来就不足以完成 FULL-PLAN 需 156 轮（且 COMPLETE_WITH_MANUAL_REPAIR）的任务；本轨迹的目的是机制验收，不是应用验收。

## 用量与缓存（首个 Responses 端点正确落账）

36 请求：input 472,373 tokens（cache_hit 114,815 / miss 357,558，**hit 率 24.3%**），output 24,143。金额为 relay 常量估计，非供应商账单。

由此发现并修复第三处 runner 缺陷：该端点的缓存桶在 `usage.input_tokens_details.cached_tokens`（嵌套），严格 schema 原只认顶层 `cached_input_tokens`，导致首批尝试全 unknown——已改为显式桶路径（嵌套＋旧扁平两形状，缺失仍 unknown 不补零），匹配形状记录进每条 attempt（`usage_shape`）。这是审查"usage 字段按实际端点解释"的直接落实。

## 本日 runner 缺陷清单（全部实战暴露、修复并有回归）

1. **相对 `--campaign-dir` 泄漏相对路径进子进程命令**（s1 首跑 child 启动即死；runner 退出分层正确报告 child_nonzero/10、零花费）→ `run_segment` 统一 resolve；回归钉住 metadata.child_command 的绝对路径。
2. **摘要不解包 journal 信封**（s2 摘要 rounds/tool_calls/terminals 恒 0/空，`recovery_required`→12 成死代码）→ 一次性解包后计数；回归用桩 journal 验证 exit 12 与计数。
3. **usage 缓存桶形状**（见上）→ 桶路径解析＋`usage_shape` 落账；合成嵌套形状回归。

三次中间 campaign 残留如实：`short-traj-20260919`（缺陷 1 的失败现场）、`-b`（修复提交后 HEAD 变化被身份校验拒绝——零付费拒绝，机制正确）、`-c`（缺陷 3 的 6 次 unknown attempt，$0.0118 保留 unknown 不改写）。本日总花费 ≈ **$0.149**（全部为 relay 估计值）。

## 边界

单任务单 serving 走查，非基准；KV 布局 A/B 对照（同任务同起点双臂）仍 NOT_RUN——需要先有可切换的第二种布局实现；金额正规化仍 NOT_RUN（需价格表）。90 分钟 soak 未重跑（应用负载实现无相关变更）。
