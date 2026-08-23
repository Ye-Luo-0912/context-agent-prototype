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

- [ ] 8. 定义最小词汇：ExecutionEvidence { key, outcome,
      observed_world_revision, validity, argument_digest, evidence_ref }，
      validity ∈ { Turn, WorkspaceRevision(N), Resource(path@digest) }。
      （评审 8）
- [ ] 9. 成功只读观察入 frontier：git.status/diff、fs.list digest、
      process.run 成功；同 world revision 重复调用记 RedundantEvidence。
      （评审 15）
- [ ] 10. RoundProgress 升级为 FrontierDelta { ObservedWorldChange,
      WorldInvalidatedUnknown, EvidenceAdvanced, ObligationResolved,
      RedundantEvidence, NoProgress }；Unknown 失效 ≠ 世界进展。
      （评审 9）
- [ ] 11. ConvergenceState { evidence_revision,
      actions_since_frontier_advance, recent_deltas 环形缓冲 }；软阈值
      触发 EXECUTION FRONTIER UNCHANGED 提示。（评审 10）
- [ ] 12. TASK PROGRESS 只渲染 typed 字段（身份/枚举/digest/计数）；
      raw body 一律留在 user-role/artifact 层。（评审 16）
- [ ] 13. 新指标：frontier_advances / redundant_evidence_calls /
      actions_since_frontier_advance / evidence_invalidations。（评审 30）

## P1 — RetryDomain 与失败域拆分（CONV-02）

- [ ] 14. FailureClass 与 FailureDomain 分离：ExecutableResolution /
      ResourcePath / ProjectMarker。（评审 14）
- [ ] 15. RetryDomain = 相同 precondition 下可证明等价的重试域
      （EditTarget 已有；新增 ExecutableResolution(argv0,cwd,PATH-digest)
      等）；hard refusal 仅限 provable equivalence。（评审 11/12）
- [ ] 16. 明确不做 K-strikes 名单硬封禁：listing 是有界的，PATH/扩展名/
      后续 build 都可能改变结论；跨名字投机循环用软性 debt 压制。
      （评审 11）

## P1 — Protocol Body Cache（PROTO-EVID-01）

- [ ] 17. Current-turn LRU：key=path@revision，~4 条 / 4–8 KiB，存活期
      ActiveTurn，来源 fs.read/edit echo；仅当 checkpoint 已移除正文、
      ResourceFact Fresh 且 revision 一致时复用；Known mutation 失效对应
      path，Unknown mutation 全部视为 stale；不进 Context / 不 Admit /
      不持久。（评审 17）

## P2 — 结构与后续

- [ ] 18. HostPolicySnapshot：resolve_policy → Arc<snapshot{policy,
      revision, digest}>，RwLock+单调 revision，lease/admission 绑定
      revision；并入 M12 P0。（评审 25）
- [ ] 19. Unified Surface Residency Planner：builtin + capability 共用
      一个压力预算，替代 capability 侧纯 TTL。（评审 20）
- [ ] 20. Convergence Bench：三个确定性场景 retry_domain /
      operational_evidence / protocol_body，先 scripted model 后 live
      A/C×2。（评审 29）
- [ ] 21. ROADMAP 不加 milestone：V1 candidate 前增加验收门 =
      Convergence bench green 且 longflow 无结构性 no-progress 循环。
      （评审 38）
- [ ] 22. agent-replay 可重建 Evidence Frontier；agent-conformance 增加
      frontier/checkpoint backward-compat 契约。（评审 27）
