# 阶段收尾旅程回执（2026-09-14，工作树）

对应 NEXT_TASKS「阶段收尾」：用既有 headless/host 流程验收后端主体。**没有新建评测框架**——旅程的每一环都映射到本树实际执行的既有回归；全部在本机 Windows 与/或 CI Linux（run `34786157399`，七 job 全绿）执行通过。

## 旅程映射

| 审查旅程项 | 执行证据（本树全绿） |
|---|---|
| ① headless/host 接收→转向→**停止/挂起**→**恢复**→验证→可继续 | `agent-host` `long_flow_controls_close_the_loop`（named pipe + unix socket 双端点）：start（新任务+focus+effective_config 断言 rolling/kernel_default）→ steer（纠正绑定指定 task、错误 task 拒绝）→ 独立第二任务 → 错误 task 的 steer 拒绝 → suspend（清 focus＋continue_readiness）→ activate（含 already_active 幂等）→ continue（期望 turn/task 不匹配不启动新轮）→ work.checkpoint（正式跨面 checkpoint 落 run 自己的 store）→ work.restore（同跨面事务，task plane 存活、双任务保留、continue_readiness=true）。host 全量 8+9+3+… 绿 |
| ② 外置卡片/分片 checkpoint/服务重启后恢复 | B 线真实服务进程回归（sharded checkpoint 服务重启后卡片与捕获元数据逐字节恢复）；本轮 B2 补充：pending 卡片未读时 GC/reconcile 延期不可逆删除，故障清除后元数据可读、正文可取（`b2_hydration_completeness` 3 项）。context-simple **398/398** |
| ③ 长进程真实终态；分页搜索/MCP | `process.session` 生命周期（start/poll/stop、exit 0/7/signal、连续写批界、stop 不被 poll 拖住）tool-runtime **278/278**；MCP `nextCursor` 分页发现（第二页工具、重复 cursor/失败类型化拒绝）capability-process **27/27** |
| ④ 生产链路组装请求验证 key/B0/B1 | `cache_routing_wire_acceptance`：真实 compose→runtime→provider→本地 HTTP 捕获；key 形状/稳定性/隔离、B0 稳定前缀＋B1 证据块逐项 SiblingField 映射、maintenance lane 独立键＋ExplicitOnly 零断点形、未确认端点剥除全部 cache 字段。compose 全目标 **61 绿** |
| 中断（取消）与安全 | turn 取消/安全点/维护取消/会话取消等既有回归（agent-runtime 全量绿于 CI run 34786157399） |

## 未验收（如实记录）

- 真实供应商（付费）接受/命中/费用对照＝C3 条件任务，NOT_RUN；本地 HTTP 捕获只证明客户端接线正确。
- 跨模块真实任务的** GUI 人工走查**不在本阶段（GUI 仅必要兼容）。
- 新 HEAD `44cdd6dd`（B2＋C wire 落树）的远端 CI 运行中，尚未出结果；此前 `34786157399` 全绿绑定其前一 SHA。

## 结论

后端主体旅程的每一环都有已执行、已通过的对应回归与明确入口；资源边界（会话/批次/卡片读取/pending 上限）与供应商缓存成本可解释性（键/断点/策略/账目分型）已在各自验收中钉住。按 NEXT_TASKS 停止条件——验收的是可用后端主体，不是审计项永久清零——本阶段收尾达成，转入下一主体功能切片由维护者决定。
