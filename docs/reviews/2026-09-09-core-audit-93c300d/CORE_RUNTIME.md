# Core / Runtime：持久化版本与执行表面

基线 `93c300d9b222ea9720579b86ac273e945f1964bc`。本轮为审查，未修改生产代码。

## CR-01 / P1：旧 checkpoint ACK 误清冻结后的同类欠账

位置：`crates/agent-runtime/src/actor/safepoint.rs:50–53`、`188–190`、`381–383`。

`accrue_checkpoint_debt` 仅按原因枚举去重，后台保存冻结的也是原因集合。快照冻结后，第二次同类型修改无法留下不同身份；第一个保存完成时，两个 ACK 分支均按原因集合做差，连后来的修改也被清除。

最小交错：

1. anchor revision 1 产生原因 `r`，冻结快照 S1 并开始后台写入。
2. 写入未确认时，anchor revision 2 再产生 `r`。
3. S1 确认，`checkpoint_debt` 被清空，`required_sequence == durable_sequence == 1`。
4. continuation gate 返回允许，但第二个状态从未被 S1 捕获。

不变量应是 `ACK(S_v)` 只能清除 `generation <= v` 的欠账；枚举相等不表示两次修改具有同一持久化身份。单调 snapshot sequence 无法弥补没有产生第二个 sequence 的修改。

终结路径也没有自动修复该窗口：`actor/turn.rs:2313` 先 `safe_point_resume_commit()`，`:2339` 只等待已有写入，之后没有因等待期间的同类欠账再捕获；`:2547–2552` 安装的是 RAM resume 状态。这里的影响是恢复时缺少较新的 anchor/resume/观察状态，**不是已验证文件写入被物理回滚**。

本轮隔离复制 agent-runtime 后，在 `actor/safepoint.rs` 添加私有测试模块 `review_checkpoint_debt`，使用真实 Actor 方法及可控制完成时点的 JoinHandle。实际命令：

```text
cargo test --manifest-path target/review-2026-09-09/core-runtime/runtime-probe/Cargo.toml --lib review_checkpoint_debt --target-dir target --offline -- --nocapture
```

当时编译成功，测试退出码 **1**，失败的是应当保留第二笔欠账的断言：

```text
running 1 test
after first snapshot ACK: debt=[], required=Some(1), durable=Some(1), continuation=Ok(())
the second same-reason mutation was never captured and must remain owed
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 368 filtered out
```

这是反例复现，不能写成“回归测试通过”。没有运行真实断电、完整落盘恢复或 provider 场景。原 `target` 探针源码/复制树在汇总前从磁盘消失，原因未确认；本节保留当时工具执行记录，以上路径目前不能直接复现，未凭记忆重造一个声称等价的测试模块。

修复方向：给持久化债务绑定单调修改代次，或把冻结批次从仍可积累的新欠账中分离；两条 ACK 路径共用相同退休规则。barrier 退出要证明所需代次已持久化，而不只是“当前 JoinHandle 已结束”。必要回归固定上述交错，并验证第二个实际快照内容和 gate 状态。继续由 RuntimeActor 编排、Core 验证提交/恢复。

## CR-02 / P2：Schema 过滤后的执行表面与模型请求不同

状态：静态调用链确认，未新增运行时探针。

位置：`crates/agent-runtime/src/actor/model.rs:1248–1255`、`:1373–1378`，过滤位于 `crates/agent-runtime/src/surface.rs:456–477`。

模型输入先从 `surface_plan.specs()` 装配。最终 `ready_report` 仍根据该 plan 产生，之后 `into_snapshot` 才编译 SchemaProfile 并移除不支持的工具。执行时安装过滤后的 `turn.tool_surface`，但实际 `ModelRequest.tools` 仍取先前的 `input.tool_schemas`。

例如 schema 带不支持的 `anyOf` 的已登记工具：静态登记路径 `agent-core/src/capability_admission.rs:143–191` 只验证名称/描述/大小/数量，没有编译 SchemaProfile，因此可以进入 plan。模型看得到工具，Core 按本轮执行表面拒绝；MustSurface 还可能被提前报告 Ready。

应满足 `names(ModelRequest.tools) = names(turn.tool_surface.specs)`，Ready 也必须描述同一个最终集合。现有 `surface.rs:1071–1106` 测试只检查 `into_snapshot` 的过滤结果，没有覆盖真实请求。

修复方向：在装配、预算计算和 Ready 报告之前完成 schema 编译；实际请求与执行检查消费同一最终表面。MustSurface 被 schema 拒绝时应显式报告不可满足。无需放宽 Core 校验。

## 已确认的边界与未收敛候选

- 默认 OperatorClosureOnly 的完成阻塞仍存在；TaskProgressProposal 不含 goal/constraints/completion_policy，计划勾选不产生完成权限。
- 已读 exact PASS 路径比较 verification revision、directive revision、workspace revision、tool、argument digest、host verification identity。未知 footprint 降级事实，未解决失败超限保留 omitted sentinel。
- capability invocation 使用登记时的 Core 授权，ReadOnly/StagedOnly 工作区适配器和动态输出 authority metadata 剥离仍有效；restore 采用 activation meet，没有从 checkpoint 提升当前宿主权限。
- Provider 旧三项修复：有界错误 body、Chat length → ModelOutputLimit、Responses EOF 尾帧路由检查仍在，不重报旧缺陷。
- Runtime 允许恢复 authority ancestor 合法的旧 checkpoint。`actor/restore.rs:136–152`、`agent-core/src/operation.rs:246–260` 未要求最新；现有 `tests/instance/restore.rs:163–189` 明确覆盖 epoch/last_seq 前进后恢复旧快照。本轮未重跑该现有测试，结论来自当前代码。
- 旧 checkpoint 覆盖当前 unresolved ACK debt、已完成任务累计触及 checkpoint 大小上限：只有静态候选，缺少故障/规模验证，不计入已确认数。
- Rolling maintain 与 checkpoint 的并发候选在默认 Runtime 未找到可达调度：Actor await maintain 后才 capture，后台任务写冻结 bytes，不把独立 ContextEngine 的任意并发调用当成产品路径。
- Skill 包树可并发修改时的检查/打开竞态仅作待核对候选；按用户最新范围，本轮不继续攻防实验，也不纳入工程问题排序。

## 覆盖

重点阅读 core kernel 的 admission/publication/approval/dispatch/prepared-effect/output/checkpoint/restore，port commit/rollback、authority sequence、standing grants；runtime actor 生命周期、命令、turn、tools、safepoint、restore、checkpoint/task；prompt/model 最终装配、surface、execution freshness/state、body cache、plugin 与 capability 的主要调用链；provider 错误与终止流路径；compose 构造/权限/恢复接线及相关现有测试。

未逐行穷尽所有 contracts、Core broker/plugin admission、execution obligation/convergence、capability/plugin 所有分支、Provider retry/SSE/Responses accumulator 与所有测试。不据此声明全仓、全平台或 B1/B2 验收完成。
