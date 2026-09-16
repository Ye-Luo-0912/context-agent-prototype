# 第七批回执 — QA/QB/QC/QD（6afa25df 续审四切片）

工单：`docs/NEXT_TASKS.md` 第七批；缺陷细节：本目录 `REVIEW.md` Q1–Q6。
执行日期：2026-09-17。基线：main `e851f7e2`（干净工作树，六审查文件已入库）。
提交：QB `559c6638`、QC `2221ecec`、QD `49209740`、QA `c62ce4fe`（按依赖顺序）。
执行方式：QB/QC/QD 三切片并行子代理执行、主代理逐片独立复验后提交；
QA 子代理超时失活，主代理接手完成（其 context-simple 部分改动经审阅后
保留并修复一处测试死锁，runtime 集成测试由主代理补齐）。

## QA — 冷正文从预览到消费成功（Q1）——已关闭（`c62ce4fe`）

- `PendingMaterialization` 增 `pending_cold_items: Vec<(id, card hash)>`：
  预览中被服务、且预览结束时唯一 owner 是冷定位行的 id，绑定**本次送出
  的卡片版本**。
- `has_exactly_one_owner(state, id, preview_cold)`：四类已加载 owner 之外，
  pending 定位行只在「行存在且其卡片哈希 == 预览送出的哈希」时计为一
  个 owner——「pending 里出现过同 ID」不充分；任何第二 owner 仍拒绝。
- 消费结算：`stamp_consumed` 对已验证冷 id 落入
  `access::stamp_pending_cold_consumed`——聚合 ack 计数前进＋写入有界
  （128）持久化 `PendingColdConsumed` 环（serde default，旧 checkpoint 可
  载）。该卡片版本水化时（批量 drain 与 per-id lane 两个安装点）经
  `land_pending_cold_consumptions` 把真实 tick/turn 盖到新条目上（延迟记
  账：热度/admit 不虚增、GC 世代锚不动），随后离环；死版本惰性清理。
- 拒绝语义保持：错误 materialization id、外来 id、卡片版本已变的冷 id
  仍被拒绝（各有测试钉住）。
- 回归：`batch_required_plan` 新增 4 条（ack 接受含 A 冷驻留、错误 id 拒
  绝、外来 id 拒绝、换版本拒绝——曾因测试在持锁块内再 await 锁同把
  tokio Mutex 而死锁，已修为锁外取哈希）；runtime 集成
  `cold_consumption_ack.rs`：真实 SimpleContextEngine（公开 API：ingest→
  maintain/gc→`gc_external_ttl_generations: 1` 老化→capture spilled=3/
  inline=0→restore 批次 0 全 pending）＋真实 composed runtime→
  `TurnCompleted`、三 sentinel 进入真实模型请求、回合后冷行仍可按 id 取
  回。**变异复验**：去掉冷 owner 计数→engine 级 ack 测试红＋runtime 测
  试红（失败信息即审查原文 "…without exactly one residency owner"）；去
  掉版本绑定→换版本拒绝测试红；还原→绿。
- 验收命令：`cargo test -p context-simple` 439/0；`cargo test -p
  agent-runtime --test actor` 100/0；clippy 0、fmt clean（两 crate）。

## QB — 一次模型尝试只有一个使用量结算出口（Q2＋Q3）——已关闭（`559c6638`）

- Q2：`OperationOutcome::ModelOutput` 臂把 `ModelUsed` 发布改为 ACK 之前
  经新 `settle_model_round_usage`（复用 `usage_settled`/accounted/settled
  去重通道）结算；ACK 失败分支原样保留（发 Error、清 turn、拒绝派发）。
  ModelUsed 日志写失败不声称入账：保留可改进义务＋恢复栅栏
  （`TurnCommitFailed{phase:"model_usage_settled_event"}`）。
- Q3：Chat/Responses 两个读流循环包进外层 async 块（读流与结算拆开），
  `seal_attempt_usage` 在**所有退出**（idle timeout、总字节 cap、framer/
  parse/accumulator 错、部分 sink 错、流 I/O）从同一 accumulator 取已知
  计数封入原错误；已带用量的出口（含 Cancel）去重不再相加，无 usage 保
  持 Unknown 不补零；错误分类/可重试性/取消语义不变。
- 契约零改动（沿 W4/R2 既有形状）。
- 回归：`context_ack_usage.rs` 3 条（ACK 失败→工具不执行＋错误如实＋
  usage 恰一次入账；健康路径恰一次；日志失败→`RecoveryRequired` 栅栏，
  不声称干净回合）；provider-openai 7 条矩阵（usage 后 idle timeout/
  malformed/io/cap/sink error 各断言 reported_usage、无 usage 同形错误保
  持 plain、Responses 变体、真实 `RetryingTransport` 首败＋成功各计一
  次）。**变异复验**：弃用 `seal_attempt_usage` 快照→恰 7 条新测试红、
  既有 W4 臂不受影响；还原→163/163。
- 验收命令（全程无 `OPENAI_RETRY_METRICS_FILE`，W4 惯例）：`cargo test
  -p provider-openai --lib` 163/0；`cargo test -p agent-runtime --test
  actor` 99/0；clippy 0、fmt clean。
- 取舍：stale/`Failed`/`Cancelled` 臂的既有 `let _` emit 不在本片范围且
  stale 上下文不宜复用新 helper（会误设当前 turn 栅栏），如实保留。

## QC — SDK 事件流健康与重同步代际屏障（Q4＋Q5）——已关闭（`2221ecec`）

- Q4：`LiveAsync` 把 `_eventsOverflowed` 纳入可用性判断——流已溢出时即
  使 socket 健康也不再复用原连接，下一个公开操作走既有重连路径重建
  snapshot+subscription+pump；故障连接锁外 dispose；新 reader 代际经
  `Events`（锁内读取）可见，新增只读 `EventsOverflowed`。
- Q5：连接安装、队列重建/清空、pump 启动合并为**同一 `_gate` 锁保持内
  的单一发布单元**，`snapshotApplied` 屏障（`Resynced?.Invoke` 锁外
  try/finally 完成后才开泵）；每条通知的代际复检（`_disposed`＋连接身
  份＋队列身份）与 `TryEnqueue` 同一锁内原子完成。非仅挪一行。
- 回归：新增 3 条——健康 socket 不断线溢出恢复（容量 2、三条 durable、
  服务端持续应答查询）、暂停「新 pump 就绪快照未应用」（窗口一）、
  经 internal `PumpDrillGate` seam 暂停「已读取未入队」后安装新代际
  （窗口二，双断言不受污染次序影响）。**变异复验**：M1 删屏障→窗口一
  红；M2 代际复检退化为只查 `_disposed`→窗口二红；还原→全绿。
- 验收命令：`dotnet test clients/dotnet/Agent.Client.Tests/…csproj`
  **134/134**（EventStreamTests 11/11）。mutation/审批不自动重发；旧
  reader 保持终态。

## QD — TUI worker 正常/异常退出共同收尾（Q6＋O3）——已关闭（`49209740`）

- `run_session` 循环三处 `?` 改为 outcome 捕获＋break；正常与所有错误路
  径汇合同一 cleanup：drop lane（停止接收）→stop 屏障→abort+join→回执
  分别报告 queued-never-dispatched / taken-result-unknown→worker panic 不
  吞；best-effort 终帧把回执绘给操作员（HEAD 上该行从未显示）。U5 顺序
  不回退（session 返回→终端 restore→`composed.shutdown()`）。
- O3：`CommandLedger`（提交先登记、worker 取走同步 `mark_taken`、
  Runtime 交互完成即 settle 移除、含 stop 屏障）从结构上消除下溢窗口；
  计数降为诊断，语义来自 worker 停机回执。
- 回归：新增 7 条（绘制失败/读键失败/正常 quit/排队 checkpoint 四条
  e2e＋worker 级屏障回执＋2 条 ledger 单元）。**红相（HEAD）**：异常退
  出后 checkpoint 照常落盘、"1/2 queued input(s) reached the runtime"；
  **变异**：cleanup 前按 outcome 提前 return→三条错误路径测试转红；还
  原→全绿。
- 验收命令：`cargo test -p agent-tui` 103/0＋2/0；clippy 0、fmt clean。

## 独立复验（主代理，提交前）

- QB：`cargo test -p provider-openai --lib` 163/0、`cargo test -p
  agent-runtime --test actor` 99/0（提交时点）。
- QC：`dotnet test` 全套件 134/0。
- QD：`cargo test -p agent-tui` 103/0＋2/0。
- QA：`cargo test -p context-simple` 439/0、`cargo test -p agent-runtime
  --test actor` 100/0（提交时点）。
- 提交顺序 QB→QC→QD→QA 保证 `tests/actor/main.rs` 与跨片依赖无踩踏；
  最终工作树干净。

## 限制与取舍（如实）

- QA：runtime 集成测试的「全 pending」证明取自 checkpoint 分片计数
  （spilled=3/inline=0）＋restore 批次 0 的既有语义——**不能**用
  `search_external` 探测 pending（命中即安装，W1 有界扫描语义）；冷消费
  的逐项事实环上限 128，超限丢最旧（聚合 ack 计数不受影响）。
- QA：子代理遗留的 context-simple 改动经逐行审阅后采纳；其测试中一处
  持锁跨 `.await` 再锁同一 tokio Mutex 的死锁（`--nocapture` 探针定位）
  由主代理修复。
- QB：stale 臂与 `Failed`/`Cancelled` 臂的既有 `let _` emit 未动（见
  上）；流 I/O 反例用 Content-Length 截断触发真实读错误。
- QC：`PumpDrillGate` 是唯一进入生产文件的测试 seam（internal、生产恒
  null）；窗口二在原代码上无外部确定性暂停点，红相由变异提供。
- QD：「在飞被 abort→result unknown」无独立确定性 e2e（actor 在
  compose 即启动，产品无可控内部慢 seam），由 ledger 状态机单元测试＋
  worker 首 await 前同步 mark_taken 的取消模型保证；e2e 用
  current_thread 执行器＋全预缓冲按键构造确定性。
- O1（MetricsSession 的 FullTree 把未读到编成 0）与 O2（B2 前置长度检
  查改 opened-handle＋`take(len+1)` 硬界）**未做**，按审查意见留作后续
  小切片，不阻塞主体。
- KV 本地序列验收表（审查 NEXT_ACTIONS）未开始；真实端点接受/命中/净
  费用仍 NOT_RUN，Q2/Q3 修后账目基础才完整。
