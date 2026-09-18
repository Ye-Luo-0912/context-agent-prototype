# 新分支实施任务 — 固定bcacf41b

本文件是审查补充草案，不已写入仓库。基线与完整理由见[REVIEW.md](REVIEW.md)。BR编号仅属于本报告，不重用旧H/G/S任务编号。

## 开工规则

核对分支与工作树，保护未提交修改。原十一批修复、F12修复、真实旅程、T8命中记录均保留。MERGED、CI绿、回合结束、模型自主交付、人工修复后的应用验收分开。共享contracts/ContextEngine/预算mode由一个集成人维护。

## B-1：冷页语义更新义务（BR1，先行）

**用户动作**：多页历史已外置时，用户明确替代旧要求；之后的新要求不能被更早的替代误伤。

**实施入口**：reachability.rs中的intent记录、安装应用与持久化；相关State/restore/helper只改真实需要的部分。

**先写反例**：
- 两条同条件旧记录，分两批安装，必须全部更新。
- 一次旧intent无匹配，之后同任务新建匹配决策；旧intent不得终结新记录。
- by证据暂不可读不消费未结算目标。
- 超过4000字符、末尾有保留修订，冷热结果相同。
- 中间checkpoint/restore、跨任务及已经终态的负对照。

**修复标准**：有界的target-view/因果上界、独立目标结算，未解析义务不等于无目标。源文本不足时取回或保持未知，不能用被剪掉的否定来推导终态。不全历史hydration、不无限pin。

**停止**：上述反例与现有生命周期定向测试通过；不要顺便改评分算法或开发向量库。

## A-1：失败检查点的真实捕获结果（BR2）

**用户动作**：旧维护/快照在途时，新回合失败仍保住正确的继续边界，status/cancel可响应。

**复用测试**：failure_resume.rs中的旧prepare测试与OccupiedBoundaryContext。组合F10+F12，不只各自重跑。

**时序**：旧task completion GC held → 新task anchor debt/read-first → 旧prepare held → Provider failure → 先后释放两门 → 验证新失败debt被真正捕获。

**实施**：safe_point_resume_commit返回Capturing/Deferred等明确结果；captured标记绑定sequence/debt_basis。保持旧ACK不能消费新debt。正常应CheckpointDurable后TurnFailed；取消只TurnCancelled。

**停止**：同TaskId冷恢复、无多余RecoveryRequired、无结果/效果重放。不要绕开durability gate。

## A-2：收敛诊断和最后文本轮（BR3/BR4）

**BR3**：容量用尽返回不可比较/未追踪，不当作重复证明；可合并区间先合并。实际cap、新窗口、已知窗口、桥接、新revision与下一prompt一起验证。

**BR4**：统一text-only模式；执行轮MustSurface仍严守，文本轮可解释缺失而不调用任何工具。只保留原决策上限，不加隐式额外调用，不自动task.complete。

**停止**：需要的内容仍可读，未知覆盖不伪报重复，文本收尾和安全边界都通过。不要引入强行自主完成的启发式。

## C-1：测试控制器清理与退出（BR5/BR7）

**正常/异常公共出口**：确认child是否创建，停止新request，限时请求停止，必要时只终止自己创建的树，reap并写清理结果；relay的关闭也必须受控。stdout/stderr/metadata/summary都保留失败事实。

**退出判定**：child非零、保护文件变化、recovery fence、账目不完整分别有状态；普通final不是应用oracle。无需更改Headless已有0..4语义，只不能让wrapper抹去它。

**产物保全**：新campaign排他创建；已有campaign的setup/l0不重铺seed，resume验证身份；显式重置单列操作。

**定向验证**：超时、KeyboardInterrupt、启动失败、挂起relay、正常终态、第二次setup、child非零与保护hash变化。使用本地进程和fixture，不接真实供应商。

## C-2：campaign额度与attempt账（BR6）

使用当前已有receipt和relay组织，不开发新平台：跨segment持久balance；committed/reserved/unknown明确；每次真实attempt有ID；缺字段不能变0；所有网络异常都有finally结算；多并发受预约约束；恢复沿剩余额度继续。

Profile必须记录实际URL的脱敏身份、protocol、返回model、字段语义、价格版本和有效预算。不要复制某公开价格当账户实际账单。允许UNKNOWN，并让它影响后续付费受理。

本地relay用合成usage测试：完整/部分/无usage、嵌套cache字段、累计快照、transport失败、429重试、临界末次调用、并发、跨segment重启。合成用量不得写成真实费用。

## E-1：证据归档和下一次实际测试

已有成功结果不要重写；保存manual repair前/后源码身份和实际验收，F矩阵逐项链接真实事件或NOT_EXERCISED。冻结应用/测试/oracle版本，发布经过脱敏的最小证据包而非只引用target本地路径。

优先执行A-1组合反例，再执行短的同任务恢复/纠正/交付轨迹。只有必要变更才重新执行90分钟应用soak。KV单独用同等任务、相同输入和验收做布局对照，保留缓存读写与主/维护/重试的总成本。工具撤销和文件失效必须立即生效，不能为命中率保留无效上下文。

## 建议命令（本轮未在仓库执行）

```sh
git status --short --branch
git rev-parse HEAD
cargo test -p context-simple
cargo test -p agent-runtime --test turn failure_resume
cargo test -p agent-runtime --lib execution
cargo test -p agent-runtime --lib surface
cargo test -p agent-compose --test kv_production_sequence
cargo test -p agent-tui
cargo fmt --all -- --check
cargo clippy -p context-simple -p agent-runtime -p agent-compose -p agent-tui --all-targets -- -D warnings
python scripts/doc_consistency.py
```

先跑新增精确test name再跑相关crate；上述filter名称仅对应当前已见模块，不代替检查实际运行测试数。不得直接运行现有付费runner进行“试一下”。合入前PR触发该SHA的既有CI；不借main旧run绿。

## 文档与停止条件

CURRENT只保留新分支范围、实际验证、未覆盖与下一动作；NEXT只保留开放行动。原报告链接保留，删除相互矛盾的历史“当前”。不把本报告全文再追加进去。GUI仍维护模式。

完成条件：语义不随分页/时间反转、失败快照确实吸收自己的debt、收敛分类不拿容量当证据、控制器异常路径仍归账/清理、原始回执可核对。不是审查项无限清零。
