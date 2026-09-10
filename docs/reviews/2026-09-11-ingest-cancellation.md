# W04：输入阶段 episode 压缩的取消与回滚

日期：2026-09-11。起点 HEAD：`f4dda3b45a1cf2dd80c1054da7d4a7d8795dd849`。本片修改 Runtime 输入事务的执行位置，保留工作区原有改动；未提交、推送或调用 provider。

## 用户现在能做什么

新输入触发 dynamic/Simple 引擎的 episode 压缩时，可以取消当前回合或停止 Runtime。取消回执要等压缩 future 结束、上下文恢复到输入前才返回；未入账的新输入不会取代原任务指令，也不会进入主模型。恢复无法确认时仍返回 `RecoveryRequired`，阻止后续正常修改。

## 复现与修改

`SimpleContextEngine::ingest(UserMessage)` 在 episode 轮换中等待 `run_distill`，发生在 `maintain(UserInput)` 之前。此前 `prepare_user_message` 在 Actor 命令处理中直接等待这次 ingest，尚无可取消的 operation；上一片维护取消没有覆盖这个等待点。

回归使用真实 `SimpleContextEngine`，把测试 episode 轮次设为 2，并加入实际 pinned constraint 使 episode 满足既有压缩资格。`force_episode_llm_distill` 保持 false，生产 GC、评分和 episode 默认值均未修改。门控 `BoundedCompactor` 停在实际 ingest 的压缩等待点，原指令尾部约束确实进入压缩源。

修复前执行：

```text
cargo test -p agent-runtime --test turn ingest_cancel::cancel_during_dynamic_ingest_restores_context_and_original_directive -- --exact --test-threads=1
```

结果：0 通过、1 失败，2.05 秒；取消等待超过测试的 2 秒上限，报 `cancel_turn was blocked by SimpleContextEngine::ingest compaction`。测试释放门控并停止 Actor 后才失败，避免遗留阻塞任务。

修改沿用既有输入事务与 operation：

1. `prepare_user_message` 只捕获输入前的 checkpoint。
2. 新输入的 `ingest(UserMessage)` 与 `maintain(UserInput)` 在同一 operation 中依次执行，共享 abort、join、代际校验与完成消息；继续任务没有新输入事务，因此不重复 ingest。
3. 原有成功路径在事务完成后才入账、切换 directive/证明版本和返回成功回执。ingest 错误也进入同一快照恢复路径。
4. 取消仍由 Core 先推进代际，再 abort/join，随后恢复快照。清理与回滚各沿用既有 5 秒上限；不确定结果走恢复围栏。
5. `ContextEngine::ingest` 文档明确：中断后的成功 restore 必须隔离遗留修改；远端工作无法确认时须失败，不能宣称已回滚。

## 验证

新增真实引擎回归：

- 取消等待中的 episode 压缩：ACK 前 compactor 已 drop，完整 checkpoint 与输入前逐字段相同；主模型未消费新输入，继续任务仍获得完整原指令，不重复 ingest。
- 停止等待中的 episode 压缩：停止返回前 future 已结束、完整 checkpoint 已恢复，主模型调用数不增加。
- 正常释放压缩：新输入只有一次 `Applied` 入账，episode 压缩报告一次，测试压缩用量 17/5 tokens；继续任务使用新指令且不重复报告压缩。这是门控实现返回的合成用量，不是 provider 用量。

另在现有 `BodyTrackingEngine` 中增加部分 ingest 修改后的错误注入：成功回滚后能接收下一输入；回滚失败则返回 `RecoveryRequired` 并拒绝后续修改。两者均不执行 UserInput 维护、不伪造成功入账。

真实引擎三项定向测试已通过，合计 0.05 秒。首次正常完成断言误把 `Applied`、`Consumed`、`Archived` 三个 `UserMessageAccepted` 生命周期事件全算作入账；按类型化 `InputLifecycle::Applied` 修正断言后通过，未为此改变生产行为。

扩展回归命令：

```text
cargo test -p agent-runtime --lib --test actor --test instance --test turn -- --test-threads=1
cargo clippy -p agent-runtime --all-targets -- -D warnings
```

| 检查 | 实际结果 |
|---|---|
| Runtime lib | 382 通过，0 失败；4.38 秒 |
| Runtime actor | 76 通过，0 失败；5.41 秒 |
| Runtime instance | 31 通过，0 失败；59.93 秒 |
| Runtime turn | 131 通过，0 失败；126.42 秒 |
| Clippy | 通过，无警告；15.63 秒 |

合计 **620 项通过**，包括既有陈旧输入完成消息、取消/完成竞争、提交效果保留、指令/证明版本与恢复测试。测试日志保存在本地 `target/w04-ingest-validation-20260911.log`，Clippy 日志为 `target/w04-ingest-clippy-20260911.log`。这是当前工作树的定向包验证，不是全仓或新 CI 结论。

`cargo fmt --all -- --check`、`git diff --check` 均通过；`python scripts/doc_consistency.py` 通过（13 份当前文档、链接与状态一致）。

## 范围与下一任务

本片证明本地实际 Simple 引擎在门控压缩等待中的取消/停止和事务一致性。它不测付费 provider 的服务端中止、计费或网络尾延迟，也不证明所有 ContextEngine 远端适配器均可在取消后继续复用。prepare 阶段的 checkpoint、其他种类 ingress、提交后世界副作用的回滚不在本片扩展范围；既有提交阶段恢复围栏保留。

Flash 首轮原始证据保持，三个小型样本没有触发压缩的事实不变。下一片回到 `NEXT_TASKS.md` 第 7 行，优先准备严格九份独立 host 验证覆盖声明的实际任务，固定起点、目标和产物检查后再进行有界 Flash 运行；大型跨 crate、付费 compactor 等待与同任务降本对照仍为未验收项。客户端 restore 状态低报继续留在已有 backlog。
