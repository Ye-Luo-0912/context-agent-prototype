# Execution Convergence 执行清单（2026-08-23 评审）

来源：2026-08-23 全仓复审（第 2–38 条）。本文是唯一推进清单，完成一项
勾一项；事实结论进对应 REPORT，架构语义进 EXECUTION_COHERENCE.md。
原则不变：单 orchestrator；advisory 不做 planner；Context GC 维持冻结。

## P0 — 文档与语义修正

- [x] 1. STATUS.md 分账：Execution Coherence V1 定为 Freeze Candidate；
      Execution Convergence V1 单列为活跃 P1。（评审 33）
- [x] 2. AUDIT_TODO 新增 CONV-01 / CONV-02 / PROTO-EVID-01；SCHED-03
      保持 fixed 不回开，产品级收敛由 CONV-02 跟踪。（评审 34）
- [x] 3. EXECUTION_COHERENCE 修正：turn checkpoint 的 reliable-facts 表述
      改条件式；Progress/Stall 拆成 coherence 与 convergence 两节。
      （评审 18/35）
- [x] 4. STATUS.md M12 P0 措辞：registry 已落地，下一步是 manifest →
      运维审核 → 版本化运行时准入，policy revision 绑定 authority。
      （评审 37）
- [x] 5. FailureCluster 语义修复：A→B→A 不得记成 3 个不同目标；
      计数器改为有界去重集合（cap 8）。（评审 13）
- [x] 6. schema_bytes 预计算到 ToolEntry，注册时算一次。（评审 21）
- [x] 7. longflow REPORT 自包含：补 A/C 轮数、tokens、historical/round、
      resident bytes、动机表、n=1 与 provider lower-bound 声明。
      （评审 36）

## P1 — Execution Evidence Frontier（CONV-01）

- [x] 8. 定义最小词汇：ExecutionEvidence { key, outcome,
      observed_world_revision, validity, argument_digest, evidence_ref }，
      validity ∈ { Turn, WorkspaceRevision(N), Resource(path@digest) }。
      （评审 8）——类型在 agent-contracts::context，随 ExecutionState
      持久化（serde default 向后兼容）。
- [x] 9. 成功只读观察入 frontier：git.status/diff、fs.list digest、
      process.run 成功；同 world revision 重复调用记 RedundantEvidence。
      （评审 15）——fs.list 成功输出补 path@digest 身份戳；命令运行
      （未知足迹）每轮都推进世界时钟，不构成"同版本重复"，由收敛
      债务软性压制。
- [x] 10. RoundProgress 升级为 FrontierDelta { ObservedWorldChange,
      WorldInvalidatedUnknown, EvidenceAdvanced, ObligationResolved,
      RedundantEvidence, NoProgress }；Unknown 失效 ≠ 世界进展。
      （评审 9）——只有前三种清停滞/聚类/收敛债务。
- [x] 11. ConvergenceState { evidence_revision,
      actions_since_frontier_advance, recent_deltas 环形缓冲 }；软阈值
      触发 EXECUTION FRONTIER UNCHANGED 提示。（评审 10）——阈值 5，
      TASK PROGRESS 渲染，不阻断执行。
- [x] 12. TASK PROGRESS 只渲染 typed 字段（身份/枚举/digest/计数）；
      raw body 一律留在 user-role/artifact 层。（评审 16）——新增
      operational_evidence 行（≤6 条）与 frontier_warning。
- [x] 13. 新指标：frontier_advances / redundant_evidence_calls /
      actions_since_frontier_advance / evidence_invalidations。（评审 30）
      ——`ExecutionFrontier` 事件 + agent-eval RunMetrics 聚合 +
      bundle metrics_json 输出。

## P1 — RetryDomain 与失败域拆分（CONV-02）

- [x] 14. FailureClass 与 FailureDomain 分离：ExecutableResolution /
      ResourcePath / ProjectMarker / NonDeterministic。（评审 14）——
      `ToolFailureClass::failure_domain()` + `ToolOutput::failure_domain()`
      （进程类工具的 PathNotFound 归 ExecutableResolution）。
- [x] 15. RetryDomain = 相同 precondition 下可证明等价的重试域
      （EditTarget 已有；新增 ExecutableResolution(argv0,cwd,PATH-digest)
      等）；hard refusal 仅限 provable equivalence。（评审 11/12）——
      LaunchResolutionFact：参数摘要（覆盖 argv0/cwd/env 覆盖项）+
      世界版本未推进才硬拒绝。PATH 的运行外变化不在观察模型内，是
      已记录的残余假设，由软性债务兜底。
- [x] 16. 明确不做 K-strikes 名单硬封禁：listing 是有界的，PATH/扩展名/
      后续 build 都可能改变结论；跨名字投机循环用软性 debt 压制。
      （评审 11）——决策记录在 actor/mod.rs LaunchResolutionFact 注释
      与 EXECUTION_COHERENCE.md。

## P1 — Protocol Body Cache（PROTO-EVID-01）

- [x] 17. Current-turn LRU：key=path@revision，~4 条 / 4–8 KiB，存活期
      ActiveTurn，来源 fs.read/edit echo；仅当 checkpoint 已移除正文、
      ResourceFact Fresh 且 revision 一致时复用；Known mutation 失效对应
      path，Unknown mutation 全部视为 stale；不进 Context / 不 Admit /
      不持久。（评审 17）——execution/body_cache.rs + 组装器
      RESTORED TURN BODIES 回注；Convergence Bench protocol_body 场景
      实测 restored_turn_bodies_seen=true。

## P2 — 结构与后续

- [x] 18. HostPolicySnapshot：resolve_policy → Arc<snapshot{policy,
      revision, digest}>，RwLock+单调 revision，lease/admission 绑定
      revision；并入 M12 P0。（评审 25）——快照类型在 contracts
      （JCS+FNV digest），registry 缓存 + admission 失效重建；
      租约侧绑定 revision 是 M12 准入线的下一步接线点。
- [x] 19. Unified Surface Residency Planner：builtin + capability 共用
      一个压力预算，替代 capability 侧纯 TTL。（评审 20）——
      CapabilityRegistry::gc_with_pressure：合并字节数超共享水位才冷却，
      Warm→Unloaded 闲置卸载不变；外部表面先于核心表面承担压力。
- [x] 20. Convergence Bench：三个确定性场景 retry_domain /
      operational_evidence / protocol_body，先 scripted model 后 live
      A/C×2。（评审 29）——agent-eval --convergence-bench 三场景全部
      PASS（真实 runtime + 真实工具面）；live A/C×2 属 M15 live 证据
      流程，不在本清单重复跑。
- [x] 21. ROADMAP 不加 milestone：V1 candidate 前增加验收门 =
      Convergence bench green 且 longflow 无结构性 no-progress 循环。
      （评审 38）——写入 ROADMAP Ordered route 第 6 条。
- [x] 22. agent-replay 可重建 Evidence Frontier；agent-conformance 增加
      frontier/checkpoint backward-compat 契约。（评审 27）——
      replay/frontier.rs 用 runtime 同一投影重建；conformance 三条
      serde 兼容测试（旧 checkpoint / 新状态 round-trip / 旧进度视图）。
