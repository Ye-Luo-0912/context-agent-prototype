# B：执行核心与恢复——保存、恢复与控制保持一致

唯一总队列：[NEXT_TASKS.md](../../NEXT_TASKS.md)。依据：[第三轮报告](REPORT.md)。当前源码为 `685b6bbb` 加共享未提交树；一次只交付一片，不能从裸 HEAD 忽略现有修复。

## 首片 EXEC-9：服务 Context 兑现保留检查点的正文保护

用户选择 service Context 后，可以在较新状态清理存储，仍从受保留的旧 checkpoint 读回其正文。

修 R3-01：沿现有 Context service wire 透传 recovery roots/roots_complete，并实现对应载荷的真实根解析。不支持解析或枚举失败时明确不完整，延期物理删除。不能以空列表代替不支持，也不能把正文搜索当读授权。

复用 `ContextEngine` protecting 契约、既有 Simple 规则、进程协议与现有 Runtime 根枚举；把当前 actor-side / spawned-boundary 的重复枚举收敛为同一实现时保持调用语义一致。不要引入第二套 Context 数据格式权威。

必要回归：真实服务进程中外置→旧 checkpoint 保留→Admit→reconcile/Storage GC→恢复旧 checkpoint 后正文可读；两种 context 入口结果一致；未知根时不删除。主报告已有删除反例，实施须转为保留正文的回归。

## EXEC-10：边界操作单槽准入与 restore 隔离

用户在保存、显式完成或恢复期间得到明确受理/繁忙结果；每份回执能结束，恢复后不会续接旧终局事务。

合并 R3-09/10 为一个操作生命周期切片：所有 `gc_work` spawn、完成、restore、stop 使用同一准入/所有权规则，包含 ReadOnlyCapture 和 TerminalFreeze。安装恢复状态前，拒绝或完整结算旧停靠事务，禁止把旧 prepared Context 回滚到新恢复状态。槽位已占用不能直接覆盖；不要另加通用任务队列/worker。

必要门控回归：terminal maintenance 停顿＋restore 旧 checkpoint；两个并发 checkpoint；GC/storage 等待期间 Stop。核对 Context、任务表、终局 checkpoint、事件、每份 reply 和后台任务是否结束。源码审查不代替这些动态验收。

## EXEC-8 残余：冷结果查询的读取工作和控制等待也有界

用户在长日志中查询退休任务结果时，仍可取消/停止，且不会挤住 journal 写入。

修 R3-11：基于现有 JSONL 的 bounded tail，限制总扫描字节与单行大小，超限返回当前 `BeyondJournalWindow`/不完整语义；把等待移出 Actor 命令分支。保留坏行 fail-closed、run/seq 与新者优先规则。先实现真实读取边界，不增加数据库或全仓索引。

## CTX-8 后半：Actor 消费外置背压并可恢复

用户遇到持久存储故障时，执行安全停在资源边界；修复 store 后可继续，已受理输入和已提交副作用仍可核对。

按 R3-08 续接既有任务，复用 ContextGcReport。Actor 保存阻塞事实并限制新正文生产，控制/查询通道保持可用；恢复后按批 drain，再解除背压。不能只把 flag 显示在 GUI 或盲目拒绝所有命令。输入、工具结果、完成边界三种来源都要覆盖。

## 共享所有权与验收

- B 单一合入 contracts、protocol、`command.rs`、compose 与必要 DTO 接线；A/C 给出领域规则和最小接口需求。
- C 的 COST-7 失败 usage 透传需要 B 接通 operation/事件；不因此让 C 同时改 Actor。Rolling 取料/退避计划由 A 单一编辑。
- 已修 EXEC-5/6/7/8 的有效部分保留；不重写 RuntimeActor、TaskManager、CorePort 或恢复半事务，不扩大远端权限。
- 开发定向回归，集成沿现有 CI。B1/B2 正式支持声明及真实冷恢复验收仍需相应证据，不能由本审查报告授予。
