# W04：回合维护期间取消

日期：2026-09-10。基线：`732cf93103cb7104f1961402f422e82143b8956c`，在已有未提交的 KV/provider 改动之上完成一个 Runtime 功能切片。

## 用户能做什么

在 UserInput、BeforeModel、AfterTool、AfterModel 维护已进入慢等待后取消。RuntimeActor 通过原有 operation 通道继续接收控制请求；一个回合只有一个维护操作及其阶段续接状态。

| 阶段 | 取消处理 | 回执与保留内容 |
|---|---|---|
| 新 UserInput | Core 先推进代际，停止并 join 维护，再恢复入账前 Context 快照 | 发起输入返回 Cancelled；取消命令在持久取消屏障后返回 Cancelled。未应用输入不成为新 directive、不创建成功 work receipt；下一条排队输入可继续 |
| continuation 的 UserInput | 停止并 join 维护，不重新 ingest 原指令 | 当前 directive 保留，可再次继续 |
| BeforeModel | 停止并 join 维护 | 维持既有 TurnCancelled 语义，不启动后续模型 |
| AfterTool / AfterModel | 停止并 join 维护，中止当前提交阶段 | 持久写入对应 phase 的 TurnCommitFailed 与 RecoveryRequired，回执为 RecoveryRequired；已应用工具效果、已入账观察与正文保留，后续 GC/提交不执行，不发 TurnCompleted |

维护 task 清理上限 5 秒，新输入回滚另有 5 秒上限；不能证明清理或回滚完成即 recovery fence。回执不以发出 abort 代替退出证明。停止运行时，已经进入提交的回合先沿原 operation 通道有界等待维护完成（5 秒）；仍阻塞才进入同一取消/恢复路径。停止不会继续执行排队输入。

引擎可替换契约明确：被丢弃的 maintain future 必须保留未完成折叠暂时移出的源记录并释放状态守卫；这不回滚之前的 ingest、已完成维护或外部效果。Runtime 持有事务与恢复权威。FocusChanged、Checkpoint、TaskCompleted 等既有事务入口不在本次切片范围。

## 实现

- `actor/maintenance.rs` 保存阶段、输入事务回执、可 join 的维护 task。完成仍经 OperationCompletion 和既有代际校验；过期完成不能拿走新回合的续接状态。
- `services.rs` 将新消息的事务拆为 prepare / finish：维护在两者之间运行，失败或取消仍恢复同一份快照。提交 directive 后再将新的 ExecutionState 装入活动回合，保证新指令让旧证明失效。
- `actor/turn.rs` 将提交拆为观察、AfterTool 续接、正文、AfterModel 续接与最终屏障，效果提交路径不变。
- 普通对话、原任务继续、start_work 都等 UserInput 事务结果再回执；取消的 work 请求不占成功幂等记录。

## 验证记录

- `cargo check -p agent-runtime --all-targets`：通过。
- `cargo test -p agent-runtime --test turn maintenance:: -- --test-threads=1`：8/8 通过。门控取消回执要求在 2 秒内；覆盖三个新增入口、原 BeforeModel、输入回滚失败、队列续接、原任务继续、work 取消重试和停机。
- 新增两个 actor 内部确定性竞争用例：旧 UserInput 维护完成不能消费新回合状态；AfterModel 完成与取消两种先后顺序各只有一个终态。
- 第一次扩展回归：lib 382、actor 74、instance 31 通过；turn 117 通过、11 失败。定位并修复输入事务提交后活动回合仍用旧指令版本的问题，以及停机与新增异步提交阶段的竞争。一次过早启动的重编译因旧测试仍占用 Windows 可执行文件而链接失败，随后等待旧进程退出再串行重跑。
- 首次 Clippy 指出阶段枚举大小及可合并条件分支；已用 Box 保存输入阶段并合并分支。
- 最终 `cargo test -p agent-runtime --test turn -- --test-threads=1 --quiet`：128/128 通过（126.90 秒），包括全部先前失败的完成证明、指令版本、停机与续跑用例。
- 最终 `cargo test -p agent-runtime --lib --test actor --test instance -- --test-threads=1 --quiet`：lib 382、actor 74、instance 31 全部通过，合计与 turn 共 615 项。
- `cargo clippy -p agent-runtime --all-targets -- -D warnings`：通过；之后仅调整代码注释与文档。
- `cargo fmt --all --check`、`git diff --check`、`python scripts/doc_consistency.py`：通过；文档门禁检查 13 份 live docs。

## 范围与下一步

没有调用真实 provider、重跑缓存实测、启动子 agent、提交或推送代码。保留已有用户改动和冻结证据。本次是本地 Runtime 验证，不声明新 CI、客户端或冷恢复正式支持验收。

`732cf93` 已包含 W04(P2) 零预算延期账目与 W06 长行/捕获字节/游标修复，本次核对提交后跳过重做。下一任务回原队列第 7 行：固定三类真实仓库任务的起点、目标、验收和 Flash 配置，再进行有界实测；合成缓存结果不替代真实交付质量。
