# C 份：缓存、完整成本与维护预算

目标：在产物质量、证据完整性、权限和恢复行为保持的前提下，减少每个成功任务的实际总成本，并解释主模型、维护、修复、重试分别花了多少。

**本次先交付 COST-6，一个切片后停止。** 随后 COST-7 → COST-8；原 COST-5 承担最终配对验收。依据见 [REPORT.md](REPORT.md) 的 R2-10/11 与优化节。

开工核对 CURRENT/NEXT_TASKS 顶部、工作树与源码摘要。已有 CurrentStateLast/PromptReuseBoundary/ResponsesExplicit、压缩身份事件、请求级输出 cap 和可选维护 transport 不重做。公共 contracts、Actor 事件、compose/SDK DTO 由 B 单一合入；A 管理 GC/Context/prompt，先提交最小字段与反例，不抢改文件。

## COST-6 / P2：cache read/miss/write 归一化必须保留真实含义

**用户结果：** 在不同供应商上看到可解释的缓存用量，后续费用不会把未命中输入误计为缓存写入。

- 入口：`provider-openai/src/sse.rs`、`responses.rs`、diagnostics、ModelUsage、compactor/ContextCompaction 的可选用量。
- 将 DeepSeek `prompt_cache_miss_tokens` 归到 miss；只有明确的 write 字段才填 write。分别记录 observed/derived/unreported/unsupported 的必要区别，不把缺失计数变 0。支持矩阵以 endpoint/model/protocol 为单位，不因 OpenAI-compatible 猜扩展能力。
- 正常 Responses transport 读取明确报告的 cache-write，不能只有 diagnostics 看得到；主调用和压缩调用按同样字段/来源透传。旧已错误写入的事件不可静默重新解释；必要时标 schema/provenance，禁止把旧字段当新可信费用。
- 必要 fixture：100 input/80 hit/20 miss/没有 write → write=None；独立 write；只给部分计数；两种 hit 拼写并存；非法/溢出/不一致计数；Chat/Responses、旧 JSON、跨语言 roundtrip。明确每种供应商分桶的包含关系，避免重复求和。
- 定向验证：provider parser/wire tests、contracts 兼容测试及消费 fixture。这里不需要真实付费调用。
- 停止条件：字段语义正确且从 transport 到消费面无损；有缓存读不代表已经得出货币节省。

## COST-7 / P2：全调用与所有终止路径的费用完整性

**用户结果：** 模型或维护失败、取消、晚到时，账目说明已知费用与未知部分，不把失败成本隐藏成优化收益。

- 入口：Dynamic pending_compactions、compactor 空输出/失败、Actor stale/maintenance cancellation、retry observer、eval/GUI aggregate。
- 给每个逻辑调用/attempt 保留 role 与稳定身份；main 和独立 maintenance transport 的局部 call_seq 不能在同一日志里碰撞。尽量复用现有 event/journal，不建账单数据库。
- 费用记录与业务提交分开：stale 模型结果不能推进任务，但有效 usage 可按原身份补充 unknown；处理重复到达、cancel 与完成同到，确保一笔费用只入一次。取消整个维护 Future 后也不能让已发生调用从账目消失。
- 移除 Dynamic 的“非零才发事件”；空摘要的已知 usage 保留；逐字段已知值保留下界。全局成本完整性要聚合 maintenance 的 unknown/estimated/retries，不能只观察 main ModelUsed。
- 必要回归：Dynamic/rolling 同型失败、只有 maintenance 有未知、空摘要但已报 usage、provider 终止丢 usage、取消后晚到、重复补账、两个 transport 的同序号、断线/快照水位后的汇总范围。不得把业务事件回放成新调用。
- 停止条件：请求开始/终止/补充可对账，未知比例与账目覆盖范围可查询；GUI 数字不只依赖当前连接收到的事件。

## COST-8 / 产品增量：真正可配置的执行段维护预算

**用户结果：** 用户能限制长期任务的压缩支出和等待时间；额度用尽时 Agent 可恢复地让出，存储故障不引发重复昂贵压缩。

- 入口：compose/host/TUI profile、BoundedCompactor、Rolling/Dynamic 触发点、既有 RoundBudget/维护报表。请求级输出 cap 和独立 timeout 已有实现，应复用。
- 将 count/token/time/retry 预算接到产品配置并进入可核对的 profile/快照说明；明确单次尝试、逻辑调用、一次 maintain、一个执行段的边界。当前 `max_compactor_tokens_per_maintain=u64::MAX` 不能被写成默认总费用已受控。
- 硬额度在调用前预留（包括有界输入与最大输出/允许重试），返回后按实际 usage 结算；unknown 不返还成已知零。若只提供软阈值允许一次超额，必须显式命名并报告；不能宣称严格上限。
- 同一有效 source/profile 的失败设置有界重试/退避和重开条件，避免每个 BeforeModel/Checkpoint 再打同一请求。变化的原文/指令/模型 profile 必须使复用失效。保源不丢、GC 不无限停、额度耗尽明确延期/让出。
- 先在同主模型下调合理输出和维护频率；轻量模型切换只在语义回归与明确配置后考虑，不擅自改用户主模型。摘要要保存真正未闭合的要求，不能以字符更短为唯一成功标准。
- 必要回归：配置到 wire/engine 的真实接线；零额度不发送；临界额度、重试、unknown、多个维护触发、冷恢复后的预算语义；持续失败后的恢复；维护被取消时费用/源正文仍可解释。
- 停止条件：配置实际生效、上限定义清楚、长期记忆与取消恢复通过。没有收益时保留原默认并记录负结果。

## COST-5 继续：三份任务的共同验收

复用既有 compose/runtime/eval/kv_cache_walk；不重开冻结 M15，不新建评测平台。本轮只安排，真实调用留给后续明确选择环境和请求/时间/输出/费用上限的执行。

固定源树、权限、模型/端点和验收脚本，至少覆盖：小修复、多文件跨 crate、跨 episode/故障/取消/冷恢复长任务。对每项优化分别做同起点配对，隔离缓存预热与顺序，再评估组合；失败样本保留在统计中，样本量限制如实写明。

| 必须记录 | 用途 |
|---|---|
| 产物/约束正确性，required miss，失效正文暴露 | 防止用证据丢失换 Token 下降 |
| 重复读/重复验证/repair，主/维护/重试调用数 | 定位执行浪费 |
| input/output/cache read/miss/write，known/unknown/estimated | 建立不重叠的计费分桶与完整性 |
| p50/p95、取消确认、GC 计划/IO/提交工作量 | 检查响应和吞吐 |
| Resident/Warm/Pending/Stored 索引与 scope 数、RSS、checkpoint 大小 | 检查总资源，而非只看 prompt 或 Warm |
| 64/65/66 任务完成，长期故障恢复，旧结果查询 | 检查跨模块组合契约 |

只有同质量成功任务的**全成本**确实下降，且取消/恢复/资源不退化，才宣布降本通过。仅 Token 改善就只声明 Token 改善；费率/usage 不完整标下界或未知。未知费用不得当 0，缓存命中率和本地字节前缀不得直接换算成本节省。

共享框架之外的优化不进入本轮：不冻结 Focus/旧证据/工具能力、不增 filler、不无限保留历史、不绕审批或改变完成权。
