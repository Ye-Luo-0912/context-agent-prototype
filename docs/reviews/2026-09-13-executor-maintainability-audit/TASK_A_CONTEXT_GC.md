# A：上下文与 GC——保留准确依据，恢复后仍可用

唯一总队列：[NEXT_TASKS.md](../../NEXT_TASKS.md)。依据：[第三轮报告](REPORT.md)。当前源码为 `685b6bbb` 加共享未提交树；执行前检查当前 HEAD 和修改状态，不覆盖已有实现。一次交付一片，先写用户结果，再改功能并做必要回归。

## 首片 CTX-10：scope 退休后仍可召回和恢复

用户可以关闭一个工作阶段、让旧 scope 退休，再通过当前任务的证据根召回正文，保存并恢复同一状态。

- 修 R3-02：当前 owner 明确释放的 scope 不得被旧 blob 重新带回。legacy 缺字段与显式释放必须有可判别语义；复用已有 merge/restore 校验。
- 同一边界收口 R3-04：退休环保留最新事实；有界诊断环不担任已完成任务禁止自动召回的唯一权威。仍保留的正文必须维持已有完成语义。
- 必要验证：真实外置→Focus/Task close→scope 退休→根召回→checkpoint/restore；超过 512 条退休记录之后最新事实与老完成正文的行为。A 线不自行改 Runtime TaskManager 权威。

## CTX-2 残余：撤销具体要求需要足够依据

用户可以在同一文件上补充兼容要求，例如修改超时日志，同时保留五秒超时。

针对 R3-03 收窄永久 Superseded 条件。复用现有 decision identity/终态通道；不能以共同内容词、文件名或相似短语自动证明撤销。无法确认时 Live 共存，必要的相关性调整保持在检索/注意力层。删除失去用途的启发式分支，避免继续增加词表。

必要验证：新反例、明确撤销、跨任务不撤销、Resident/Warm/Pending/Stored 相同语义；不以另外几个英文模板通过宣称自然语言语义已被证明。

## CTX-11：材料化复用四种 owner 与当前元数据规则

用户当前要编辑的文件，无论正文在何处，都可被正确投影；当前 pinned/作用域事实在 prompt 和 fetch 中一致。

合并 R3-05/06 为一片：统一读取计划所需的 owner 解析和 Stored merge，不重写评分/Frame 编译器。foreground/required 保持各自上限、范围、版本、权限与选择顺序，投影不变成 Admit。尽量复用 `catalog_body`、现有 CatalogLocation 与修准后的 `reattach_owner_metadata`，不新增一份正文目录或生命周期权威。

必要验证：同一正文在四个位置的 foreground/required/fetch；Stored 元数据经提升/保留调整后，检查最终 `MaterializedContext`。并复跑 CTX-10 的退休后读回边界。

## CTX-12：召回数量预算不被无效候选消耗

用户可以在无关工具输出之后找回相关旧证据。修 R3-07：数量额度扣在真正召回处；如需扫描上限，使用已有批次/游标并与数量额度区分。保持默认 GC、评分和 anchor 权限语义。

验证无效 Warm 候选前置/后置、Stored 有效候选、无命中时的有界工作；不要只断言计数器。

## 接口与剩余项

- CTX-8 的 Runtime 背压由 B 线接完，A 线仅维护报告的真实语义和故障恢复行为，不另造执行控制器。
- ExternalMap/Catalog/checkpoint 历史元数据有界仍沿 CTX-9/N7 后续，不因 scope 节点已退休就关闭。先测当前路径再选择最小存储/分页改动。
- `context-baselines` 的预算/摘要状态接口仍由 A 维护；C 提出具体成本请求，避免两线同时编辑 rolling。
- 共享 contracts/protocol/command/compose 由 B 单一合入，A/C 提交小接口需求。不要动冻结实验、provider 默认或无关 GUI。
