# Trust & Obligation Program（2026-08-23 第二轮复审）

Status: completed 2026-08-23（全部 22 项 [x]）。代码落在
PROMPT-AUTH-01 / EXEC-EVID-01 / PROTO-EVID-02a/b / CAP-OBS-01 路由净化 /
CONV-03 Obligation Ledger / EVAL-IMMUTABLE-01；缺陷账目与残余工作见
[`AUDIT_TODO.md`](AUDIT_TODO.md)，架构语义见
[`EXECUTION_COHERENCE.md`](EXECUTION_COHERENCE.md)。

来源：2026-08-23 收敛落地后的第二轮全仓复审（第 4–31 条）。上一份清单
已更名 [`EXECUTION_CONVERGENCE_V1.md`](EXECUTION_CONVERGENCE_V1.md)
（历史落地记录）；确认缺陷进 [`AUDIT_TODO.md`](AUDIT_TODO.md)。原则
不变：advisory 不是 planner；Context GC 冻结；模型保留全部能动性。

核心方向转变：Progress 是向量不是标量。全局前沿继续负责"整体是否在
获得新事实"；新增 **Obligation Ledger** 回答"某个已被证明的 blocker
的前置条件有没有变化"。同时堵住三个 correctness 漏洞与一个信任边界。

## P0 — 正确性修复

- [x] 1. PROMPT-AUTH-01：RESTORED TURN BODIES 从 focus_frame(System
      role) 移到 user-role 消息；回归测试断言对抗性正文永不入 System。
- [x] 2. PROTO-EVID-02a：正文缓存来源收窄为 fs.read；edit 成功只做
      失效，不再把 patch echo 冒充 exact body。
- [x] 3. fs.list 目录 digest 改为对完整 listing 计算（visible 只进
      model_content），修复分页窗口内变化被误判冗余。
- [x] 4. EXEC-EVID-01a：统一 `evidence_is_current` 谓词——
      WorkspaceRevision(n)==current；Resource 需 Fresh 事实且 digest
      全等；Turn 需当轮。projection 与清理共用同一谓词。
- [x] 5. EXEC-EVID-01b：validate_execution_state 补 evidence ≤16、
      recent_deltas ≤8、tried_targets ≤8 及字符串长度界（restore
      契约不得假定 checkpoint 可信）。
- [x] 6. 证据的 argument_digest 改用 Runtime 的真
      ArgumentDigest（经 observe_tool 传参），消除 argv/cwd 与
      limit/cursor 碰撞。

## P1 — CONV-03 Obligation-scoped Convergence

- [x] 7. ExecutionObligation 账本：只能由 typed 失效事实产生
      （ExecutableResolution / EditTarget / ResourcePath /
      ProjectMarker），key 含前置指纹；无关推进一律不清除。
- [x] 8. process.run missing 输出盖章 host-trusted
      resolution_fingerprint = cwd listing + PATH + env overrides。
- [x] 9. 解析规则：仅前置变化或同类成功解析义务；每义务有界警告行
      （TASK PROGRESS），与全局 advisory 正交。
- [x] 10. 双层职责写进 EXECUTION_COHERENCE：Global Frontier ≠ blocker
      resolution；三条新不变量（§28）。
- [x] 11. ROADMAP 收敛门替换：peak 指标降级为 diagnostics；新门 =
      bench 绿 + 无未解析义务在不变前置下超有界尝试 + hidden 绿。
- [x] 12. LaunchResolutionFact 的等价判定改用 resolution_fingerprint，
      不再用整个 workspace_revision。

## P1 — 信任边界（CAP-OBS-01）

- [x] 13. 动态 Capability 输出的信任敏感 metadata 键在路由层剥离
      （fail-closed）：path/revision/files/verification/intent/
      mutates_workspace/failure_class/recovery_hint/_runtime。
- [x] 14. ToolExecutionFacts 契约方向写入 TOOL_RESULT_ENVELOPE 信任节
      （§30）：Producer metadata ≠ trusted runtime facts；
      ToolOutput = 低权限模型载荷。完整消费方切换为后续主线。
- [x] 15. 明确不动 Tool Surface utility（§20），等 Obligation 稳定。

## P1 — 评估可审计性

- [x] 16. EVAL-IMMUTABLE-01：pair sink 已存在目录拒绝隐式覆盖，改写
      attempt-N；失败尝试保留。（§22）
- [x] 17. PROTO-EVID-02b：缓存可观测计数（eligible/hit/miss/
      invalidated/oversize/restored）以事件出账，报告可独立验证。
      （§13）

## 文档与卫生

- [x] 18. STATUS.md 刷新（§25）：删"next evidence step"/"conditional"
      旧话；记录 clean n=2 结果与新分账。
- [x] 19. AUDIT_TODO：CONV-01/02/PROTO-EVID-01 归档 Closed；新增
      PROMPT-AUTH-01 / EXEC-EVID-01 / CAP-OBS-01 / PROTO-EVID-02 /
      CONV-03 / EVAL-IMMUTABLE-01。（§26）
- [x] 20. 本文件取代旧 TODO 的活跃地位；旧文件改名 V1 历史。（§27）
- [x] 21. M12 备注：不做全局 revision 失效，改 per-binding
      （tool_name + policy_digest）粒度；Tool Surface utility 推迟。
      （§20/21）
- [x] 22. 删除空文件 cmp_report.sh；CI 结论以真实 Actions 为准。
      （§31）
