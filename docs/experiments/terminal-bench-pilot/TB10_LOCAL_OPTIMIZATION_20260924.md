# TB10 本地优化回执（2026-09-24）

用户要求落实此前选定的三项优化：修复 TB9 暴露的授权耗尽空转、减少过早的回合历史压缩，并让已结算工具观察在长回合内进入可检索 Context。工作基线 `ece82e04fee63625675248fb80d638b02cdcdd26`；保留共享工作树原有的未提交改动。此轮是本地代码及零供应商验证，未打开付费窗口，未修改 TB9 冻结产物。

## 行为变化

1. **授权结果有来源且有界。** `TaskApprovalGate` 在消费或拒绝 grant 的同一次锁内操作中产生 `grant_exhausted` 事实，包含 grant ID、`used`、`max_runs`；旧 `ApprovalGate::authorize` 仍可用，其他 gate 通过默认详细入口兼容。Core 将事实加入模型可见拒绝和类型化 metadata，并从已获批准的工具结果中剥离同名保留字段，阻止工具伪造 Core 拒绝。Runtime 观察同一已耗尽 grant 的重复拒绝，达到 16 次时保留已结算结果并使用现有文本终轮机制；TaskCompleted 不会由此产生，下一条合法只读调用仍可执行。
2. **准入检查与预算内历史。** TB9 runner 在启动前验证独立签署的 `python3` grant 至少覆盖决策数，原有 64 次 grant 对 400 轮窗口直接拒绝；该检查是准入下界，不会生成或扩大授权。Runtime 只使用 provider 发送窗口超出 Context 打包窗口的余量延长完整工具交互尾部，已占用的 Context 工作集额度不被历史挪用。预算估算、`ContextHints` 的正文可见性、PromptAssembler 实际组装与最终账目使用同一 `retained_turn_exchanges` 值；无余量时保持原六组投影，最终发送预算门仍保留。
3. **长回合增量摄入。** 已结算且应持久化的工具观察在现有异步 BeforeModel 通道摄入，再由下一次模型请求 materialize。TurnFrame 仍保留完整审计；正常结束和取消仅摄入未处理后缀。摄入批次先捕获 Context 回滚基准，取消先中止/等待并恢复；恢复不能确认时进入 RecoveryRequired。保持原工具 scope 身份，不新增只读工具强制检查点。SimpleContextEngine 现有同任务最近文件正文路径与版本选择机制在实际 `fs.read` 结果携带可信资源身份时能从已摄入观察选回相关正文。

附带修复一处测试暴露的独立有界输出缺陷：插件检查的输出尾部在添加“子进程树清理未确认”提示后重新裁剪，并在句柄丢弃时终止子进程。旧测试原本在本机两次复现超界，修复后定向通过。

## 行为证据

- Core 实授 grant 测试：2 次允许后第 3 次返回 `GrantExhausted { used: 2, max_runs: 2 }`，没有重复调用底层 gate。真实 Runtime 测试：1 次进程执行获准，随后同一 grant 的拒绝有界收尾；中途的只读调用仍成功，任务未被错误完成；工具试图伪造 grant 耗尽字段会在 Core 边界被剥离。
- Runtime 发送窗口测试：12 个小型工具结果在有 8K 发送余量时完整出现在实际模型请求；没有额外余量时仍采用六组尾部与 checkpoint 摘要；一个较大的历史结果不能挤进很小的余量。
- 真实 SimpleContextEngine 测试：修改前连续 13 次 `ContextPrepared` 始终只有 1 条初始 item、`tool_round=0`；修改后下一次请求前观察可见并能选中相关正文，回合最终及 Context 冷恢复都只记 12 次，不重复写入。
- 门控取消：第二条观察等待中取消时，先中止并恢复已部分摄入的批次，再对两条 Core 已结算结果各保存一次；故意令 restore 失败时取消不返回成功，状态为 RecoveryRequired。
- 工具 scope 的原始生产者身份及未确认副作用的 Core 权限和提交门保持原有约束。历史工具正文继续位于低权限消息层。

## 已执行验证与限制

`python -m unittest discover -s scripts/tests -p test_terminal_bench*.py`：13 passed（含 64/400 准入反例）。
`cargo test -p agent-core -p agent-runtime --lib --quiet`：Core 158 passed；Runtime 452 passed。
`cargo test -p agent-runtime --test turn --quiet`：171 passed、1 ignored（最后一次整组结果）。
`cargo test -p agent-compose --lib --quiet`：43 passed。
`cargo clippy -p agent-contracts -p agent-core -p agent-runtime -p context-simple -p tool-runtime -p agent-compose -p agent-tui --all-targets -- -D warnings` 与 `cargo fmt --all --check`：通过。
受影响生产代码最终在 Debian12 容器内完成 `cargo build -p agent-tui --target x86_64-unknown-linux-gnu`；该 ELF 再以只读 bind mount 放入无网络 Debian12 容器，`agent-tui --help` 退出 0。新二进制 SHA256：`285988df77924c1286579ca206986a62dd056f2cc5d4d084dd527d8e3ca5a4fc`。构建产物位于工作区忽略的 `target-debian12/x86_64-unknown-linux-gnu/debug/agent-tui`，旧 TB9 二进制与模型证据未被改写。
前一实现曾把提前摄入的结果改归已关闭 Tool scope 的父 Focus，破坏了取消/冷恢复的原始 scope 身份回归，已完整撤回；最终 Context 源码无本轮改动。最终方案保留 producing scope，通过真实 `fs.read` 的可信路径/版本事实让现有 SimpleContextEngine 从本任务最新文件正文中选回观察。相关 Runtime turn 全组及部分摄入取消/回滚失败用例在最终代码上通过。

本轮没有把旧 4/18 或模型任务质量归到本地代码修复。授权额度仍由操作者提供；原 TB9 的 64 次配置现在会被新 runner 拒绝，修复没有自动续配。历史保留策略仅在声明的 provider 发送余量存在时扩展，不能宣称所有 serving 的记忆或费用都会改善。relay 累计输入预约仍按请求字节数除以四估算，没有服务端 tokenizer 的可证明上界，不宣称严格供应商 input token/金额硬封顶。真供应商质量、费用、远端 CI 以及跨进程长期负载仍未在此轮执行。未提交、未推送。
