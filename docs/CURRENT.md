# 当前事实与工作范围

本页是当前状态入口。执行任务及状态只维护在 [NEXT_TASKS.md](NEXT_TASKS.md)。旧报告中的"当前"只属于其固定基线。

## 已核对基线

- 阶段审查基线：`4aaa8bea89336e2ec0fd21c76d04967814b24020`（2026-09-14）。该 SHA 的 CI run `34788369834` 最终 success，**第 2 次尝试**（满载抖动重跑），非首次全绿。审查报告：[下一阶段审查](reviews/2026-09-14-next-stage-review-4aaa8bea/REVIEW.md)。
- main 之上另有并行分支在飞（如 `codex/headless-output-budget`：真实 DeepSeek 任务记录 headless 输出缺口、失败轮结算与输出预算修复）。采用任何结论前核对实际分支与 HEAD。

**2026-09-15 续审（基线 `258eb4eb`）发现并已修复 S1**：T4 二期的降级回归测试曾持 state 锁调用 fetch_external（内部重取同一锁），确定性自锁导致 CI run `34917761534` 两 job 取消——已修（锁分段＋集合断言），[续审报告](reviews/2026-09-15-continuation-review-258eb4eb/REVIEW.md)。续审同时确认：T1 统一装箱/T2 增量目录/T3 有界发现/T6 键编码已有实现保留不重开；新开 S2a（装箱身份/覆盖一致性）、S2b（官方缓存 mapper 形状修正——本地可完成不等付费实验）、S3（固定预算冷目录闭环）、S4（T7 独立进程＋指令传递证据）。

**2026-09-15 进展**：第一批 T1/T2/T3 与第二批 T4/T6/T5 已全部合入 main（T1 统一装箱＋诚实降级；T2 增量目录维护；T3 有界进程与发现；T4 冷目录预算化第一期；T5 一份有效配置；T6 键编码迁移＋断点形状 fixture）。各片本地全量绿＋clippy 0，远端 CI 以 run 记录为准。

**2026-09-15 续审批次收口**：S1/S2a/S2b/S3/S4 全部关闭（见 [NEXT_TASKS.md](NEXT_TASKS.md) 对应行与回执）。S3 固定预算冷目录闭环（per-read deadline、字节预留、保 claim 访问戳、类型化背压、搜索 coverage+continuation 进模型正文）；S4 独立进程变体＋指令传递证据（真实两进程、脚本 provider 内容门禁）。同批修复三处满载 Windows 抖动：t7 旅程审批交付后的立即断言（`4ba60b97`）、cold_bounds 捕获 io 墙钟预算 2s 过窄（随 S3 关闭）、host_e2e stop 唤醒在死管道上盲转 panic（`48ddcb1f`）。残余限制如实记录在两份回执（wire 层 coverage 传播、续查 token 不进 checkpoint、pending 目录随历史增长）。T8 仍为条件任务（付费实验），无新开任务。

**2026-09-16 新审查（基线 `4f6eb7ff`，CI run `35019429861` 首次成功）**：开 V1–V7 七项发现，核心主线是"分页只改变驻留位置、不改变语义身份与保护义务；取消只改变执行结果、不抹掉已知成本"。最高优先级 V1（P1）：scope 退休扫描看不到未加载冷页的 scope 引用，可破坏可达性/恢复目录。四切片 W1–W4 已入 [NEXT_TASKS.md](NEXT_TASKS.md) 第四批（V1 先行，其余可并行），报告与覆盖表见 [docs/reviews/2026-09-16-review-4f6eb7ff/](reviews/2026-09-16-review-4f6eb7ff/REVIEW.md)。审查环境未执行本地测试，全部回归待实现时红-first 补齐。

## 当前阶段：可持续使用的后端开发流程

目标：**同一 Agent 在同一任务与工作区内，持续完成计划、检索、修改、验证、中途纠正、中断、冷恢复和交付；热资源、维护工作和供应商缓存成本有明确边界，核心规则在少数实现入口维护。**

不是"继续关闭审查项"，也不是全仓重写。每推进一个主体功能，同步消除该功能涉及的重复决策、隐式约定和状态分歧（可维护性是切片验收条件）。GUI 维持必要兼容，不扩展功能。

三线不变：**A 执行核心与工具；B Context/GC/搜索；C 平台/供应商 KV 与成本。** 引用历史问题时带报告日期与原始编号。

## 上一阶段成果（已关闭，回执可查）

- 文档入口已分离职责；文档检查只验证机械结构。[应用回执](reviews/2026-09-14-docs-entry-review-2b43186b/APPLICATION_RECEIPT.md)
- B 线恢复数据保全（N01–N03）＋ hydration 完整性传播（B2）：pending owner 保全、有界/校验卡片读取、删除许可=根完整∧元数据完整。[B 线回执](reviews/2026-09-14-backend-review-6eda2474/B_LINE_RECEIPT.md)
- A 线 session 终态事实化/批次硬界/每会话锁、grace 退出时重置、MCP 分页发现。[A 线回执](reviews/2026-09-14-backend-review-6eda2474/A_LINE_A1_A2_A3_IMPLEMENTATION.md)
- C 线 KV 接线与真实链路 wire 验收（本地 HTTP 捕获，非供应商校验）。[C 线回执](reviews/2026-09-14-backend-review-6eda2474/C_LINE_C1_C2_IMPLEMENTATION.md)
- 阶段收尾旅程：各环映射到已执行的全绿回归。[旅程回执](reviews/2026-09-14-backend-review-6eda2474/STAGE_CLOSING_JOURNEY_RECEIPT.md)

## 当前限制（如实）

- 跨进程连续任务轨迹已由 `host_process_variant` 证明（真实两 OS 进程、同 TaskId/lineage 恢复、指令传递证据）；仍未覆盖 watchdog/监督重初始化的全部路径。
- 本地 HTTP 捕获只证明客户端发出了字段；端点 schema 接受、实际命中、任务净成本下降均未验证（T6/T8；V6 工具结果块类型是 W3 待修项）。
- 冷目录分页与旧路径的跨层缺口未收口：scope 退休可漏未加载冷页引用（V1）、必需正文可被误报 Missing（V2）、service 边界丢 coverage/续查（V3）、续查状态可膨胀与 ABA（V4/V5）、取消丢已知用量（V7）——W1–W4 队列见 [NEXT_TASKS.md](NEXT_TASKS.md)。
- 正式 `agent-host` 未指定策略时仍默认 Rolling；Dynamic 是可选实现。配置依据 [CONFIGURATION.md](CONFIGURATION.md)。
- 尚不能宣称：无限历史热内存有界、全部源码逐行审查完成、供应商 KV 已实测降低任务费用。真实模型实验按预算和凭据条件执行，不阻塞无须模型的生产接线。

## 按需阅读

架构边界：[ARCHITECTURE.md](ARCHITECTURE.md)；上下文规则：[CONTEXT_LIFECYCLE.md](CONTEXT_LIFECYCLE.md)；恢复操作：[RECOVERY_RUNBOOK.md](RECOVERY_RUNBOOK.md)。只读当前任务相关部分。

历史报告及冻结证据保留原位置。旧 `state.json`（v2）只作导航/来源元数据，不参与当前派工。
