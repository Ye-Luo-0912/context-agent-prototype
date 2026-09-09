# 2026-09-09 全仓范围续审：核心状态、上下文与工程边界

基线：`93c300d9b222ea9720579b86ac273e945f1964bc`。

结论：现有 Core / Runtime / Context 分层值得保留，当前优先问题在持久化确认、所有权、正文覆盖和生命周期组合。主表收录 **14 项工程问题：7 个 P1、7 个 P2；其中 13 项有本轮动态反例，1 项为静态调用链确认**。这不是“全仓逐行审完”或“所有缺陷清零”的声明。按用户最新要求，网络安全不作为本轮深入方向；已取得的文件日志完整性证据留在 IO 附录，未继续攻防实验。

本轮只新增审查文档、独立反例源码和源码清单；没有修复生产代码、改变架构、提交、发布或更新 M17 工单状态。既有 A1/A2/A3/B1/B2 等修复定向确认后跳过，下述残余均以当前 HEAD 为准。

## 按影响排序的发现

P1：应优先处理的持久状态、数据或主要执行路径问题；P2：有具体触发条件的功能/进展/资源问题。优先级不表示已经完成相应平台验收。

| 编号 | 级别 | 具体问题与后果 | 主要定位 | 证据 |
|---|---|---|---|---|
| R01 | P1 | 后台快照确认按原因清债，误清冻结后的同类修改；允许继续但新状态未持久化 | `actor/safepoint.rs:188–190,381–383` | 私有 Actor 反例，安全断言失败；[Core](CORE_RUNTIME.md) |
| R02 | P1 | pending 外置记录漏入强引用根，其仍引用的证据被 Storage GC 删除；仅剩 pending 时还停止重试 | `context-simple/store.rs:1026–1036`、`gc/full/mod.rs:91–99` | 真实 blob 删除、对照组保留；[Context](CONTEXT.md) |
| R03 | P1 | reconcile 因新快照已有 Resident 副本删除 blob，破坏仍受支持的旧快照恢复 | `context-simple/store.rs:1439–1450` | restore B → 清理 → restore A → fetch None；[Context](CONTEXT.md) |
| R04 | P1 | supervision 锁把单次退避量当总等待量，超时条件永远不可达 | `tool-runtime/supervision.rs:79–91` | Windows/Linux 持锁 4 秒仍等待，释放后才返回；[IO](IO_TOOLS.md) |
| R05 | P1 | session 输出 EOF 被当成退出，poll 无期限等待活进程且不响应取消，持有全表锁 | `tool-runtime/tools/session.rs:81–88,555–561` | 自有进程关闭输出后继续运行；取消后 2 秒 poll 仍未返回；[IO](IO_TOOLS.md) |
| R06 | P1 | 合法 2,001 字符 goal 不能被 .NET snapshot/task detail 接受，后续握手持续失败 | `Agent.Client/WorkDto.cs:306,587` | scripted wire submit 接受、两次 snapshot fault；[平台](PLATFORM.md) |
| R07 | P1 | 并发刷新或已发请求超时会清掉未知提交的幂等键；同目标重试变成新受理身份 | `MainWindowViewModel.cs:736–749,790–795` | 两条路径均 calls=2、same id=False；未执行真实重复副作用；[平台](PLATFORM.md) |
| R08 | P2 | Rolling 仅给压缩器 2,000 字符，却移走整批旧记录并宣称覆盖，未读尾部也退出工作集 | `context-baselines/rolling.rs:156–180` | 默认配置、compactor 输入与 engine checkpoint 双检查；[Context](CONTEXT.md) |
| R09 | P2 | 同版本不交叠 fs.read 窗口共享 path@revision，历史互补正文被误省略 | `prompt.rs:556–580`、`materializer.rs:836–850` | 100 行历史 body 从可见变为只有 descriptor；[Context](CONTEXT.md) |
| R10 | P2 | Schema 过滤仅影响执行快照，实际请求仍向模型展示被移除工具，Ready 与执行集合不同 | `actor/model.rs:1248–1255,1377` | 静态调用链确认，未新增动态探针；[Core](CORE_RUNTIME.md) |
| R11 | P2 | session start 早退泄漏 Pending 槽，16 次失败后无进程也不能再启动 | `tool-runtime/tools/session.rs:323,336–341` | Windows/Linux 16 个不存在程序 → 第 17 个合法程序被拒绝；[IO](IO_TOOLS.md) |
| R12 | P2 | after_tx 在同事务 Prepared 处越过游标，又返回同 ID 的 Committed，增量读取不前进 | `agent-workspace/lib.rs:1437–1443` | Windows/Linux 无新增写入仍返回最后提交；[IO](IO_TOOLS.md) |
| R13 | P2 | session 事件队列 overflow 后永久 completed；snapshot/底层重连成功也无法恢复新事件 | `Agent.Client/ResumableSession.cs:267–281` | connected=True、connections=2、new event delivered=False；[平台](PLATFORM.md) |
| R14 | P2 | 每事件 UI.Post 把有界源转成无界 dispatcher backlog，输出上限过晚生效 | `MainWindowViewModel.cs:483–494` | 暂停 UI 后排队 10,000 个 1KiB delta，源 backlog=0；[平台](PLATFORM.md) |

这些反例分别证明相应方法/库路径的行为，不等于都已运行整个宿主、GUI、真实模型和冷恢复链。原临时工件消失、现存反例及实际命令见 [EVIDENCE.md](EVIDENCE.md)。

## 数学与工程化设计判断

### 1. 状态合法不等于状态变迁合法

保留 RuntimeActor 唯一编排、Core 唯一审批/提交/恢复权威。进一步把跨异步阶段的确认绑定到产生它的身份和代次，而不是仅比较标签。R01 的根因是两次不同修改都映射为同一个 enum，ACK 已无法区分；R07 则在没有关联受理证据时丢掉了请求身份。

```text
ACK(snapshot_v) 只能退休 generation ≤ v 的欠账
Pending(k) → Unknown(k) 仍保留 k
Unknown(k) → 已受理/已拒绝 必须有对应 k 的证据
```

这需要在既有类型中表达代次/状态，不需要第二套调度、数据库或权限权威。模型完成的证据继续绑定同一任务、同一检查定义以及当前 freshness；本轮没有发现足以放宽现有完成门槛的理由。

### 2. GC 必须同时证明安全与进展

工作集迁移应保留一个明确的当前逻辑 owner，允许磁盘保留多份可恢复副本。物理删除则必须检查全部保留者的强引用闭包，包括新加的 pending 所有者和仍支持恢复的 checkpoint。

```text
Delete ∩ Reach_strong(all_retained_roots) = ∅
```

这是 R02/R03 的共同缺口。已有强边 worklist 思路可继续使用：强边遍历按访问到的顶点/边工作，避免反复全表扫描；先补齐根集合，再讨论耗时预算。与此同时，IO 恢复可用后 pending 必须能进入维护并取得进展。只证明取消不丢 RAM、却不证明后续能找到/重试该记录，仍不构成可恢复系统。

### 3. 正文覆盖需要集合包含证明

文件 revision 是内容版本身份，不是读取范围。R09 应比较可见区间的并集是否覆盖历史区间；R08 应记录压缩器真正消费了哪些输入、哪些部分尚未消费或可按引用恢复。attention、semantic、residency 三维仍独立，不能用高分/命中/租约代替正文或验证证据。

```text
历史区间 ⊆ 同版本当前可见区间并集
移出的正文 ⊆ 已消费输入 ∪ 可恢复残余
```

前者可精确测试；后者保证来源覆盖，但不能保证摘要语义无损。应诚实区分这些保证。

### 4. 有界必须约束整条路径

R04 的 `w_next = min(2w,250)` 推出 `w ≤ 250`，所以 `w > 2000` 永不成立。单次退避上限不能约束累计等待，需要单调时钟 deadline。R05 同样需要区分输出 EOF 与 OS 进程退出，并在等待/锁持有处兑现取消。

R14 的内存保留量则是各级队列与 payload 的总和：`M = M_transport + M_session + M_dispatcher + M_rendered + ...`。限制其中两项不能证明总量有界。背压/批量合并应在 UI 投递前生效，durable 事件被挤压时保留明确恢复协议。R11 用 reservation guard 保证失败/取消后槽位归还，R13 明确 session 终态或新一代事件流；completed channel 不能靠 Clear 复活。

### 5. 搜索先保证可解释的完整性，再优化排序

继续采用现有 catalog/inverted index 加有界正文读取。已读路径中，Stored 读取有次数/字节界，预算不足要求收窄条件，读取失败没有伪装成完整无命中。本轮没有运行召回率/延迟实验，也没有找到引入向量库能解决的当前问题。

后续优化以候选数量、读取字节、维护成本和实际耗时为依据；明确身份搜索与正文搜索的语义。候选完备性、读取预算、结果完整性、排序质量要分别验证。不要用更复杂的相似度模型掩盖范围缺失或恢复数据不可达。

## 覆盖范围与未覆盖部分

完成 20 个 Rust workspace crate、.NET SDK/测试、桌面与脚本的目录/入口清点；深读范围按实际风险分配如下。文件索引/搜索结果不算全文阅读，相关现有测试被阅读不算本轮已执行。

| 范围 | 深度与主要路径 | 明确限制 |
|---|---|---|
| agent-core / agent-runtime / agent-contracts | admission、effect、审批、提交/恢复、actor、checkpoint、task、prompt/surface、execution freshness、body cache、capability/plugin 主要路径 | 未穷尽所有 contracts、broker、obligation/convergence、provider 请求组装所有分支和全部测试 |
| context-simple / context-baselines | ingest、materialize、所有权、GC plan/IO/commit、storage GC/reconcile、catalog/search、scope/directive/lease、Rolling/append 生产路径重点 | 未跑算法规模评测、全部故障组合或逐行复核所有旧夹具 |
| agent-workspace / agent-storage | 句柄/事务入口、journal、change feed、authority/process/remote journal、metadata 发布及事件 writer | 目录事务、WAL recover/fold 与全部恢复解析器未完整覆盖 |
| tool-runtime / agent-process / agent-capability-process | search/fs 相关路径、process/session、supervision、registry、host/capability/MCP 生命周期 | edit/patch/git/shell/code/verify/proof_runner/python/task 完整正文，以及隔离实现全部平台分支未完整覆盖 |
| context-contextcore / agent-context-service | adapter、materialization 校验、serve session/response 与 engine 路由 | wire/main 所有路径未完整覆盖 |
| provider-openai / agent-compose | 错误流/终止/EOF 路由与构造、权限、checkpoint 接线 | 未完整复核 retry/SSE/Responses accumulator、未连接真实 provider |
| agent-host / agent-platform-protocol / .NET / 桌面 | framing、会话授权、事件转发、恢复、dispatch、typed DTO、连接/刷新/提交/事件消费/资源生命周期 | 无真实全链 GUI/provider/平台验收；B4/N8 和 C3 已知未接部分不计新缺陷 |
| agent-tui / agent-replay | 共享入口、CLI lifecycle/resync、replay recovery/barrier/trace read 轻审 | 未运行实际 TUI/replay，非全文审查 |
| agent-conformance / agent-eval / scripts | conformance 角色/检查入口；eval CLI、bundle/hygiene、文件/进程操作入口；dist 脚本轻审 | 未运行冻结评测、打包、全部测试/夹具；没有声明整个仓库已形式化证明 |

详细分工证据在 [Core](CORE_RUNTIME.md)、[Context](CONTEXT.md)、[IO](IO_TOOLS.md)、[平台](PLATFORM.md)。初始清单已缺失，[最终源码清单](source-manifest.json) 为重新生成，不能据此声称前后两个清单已比对。

## 交付与建议下一切片

本轮的实际验证与环境限制见 [EVIDENCE.md](EVIDENCE.md)。没有全仓测试/远端 CI/B1/B2 正式支持验收，也没有把现有工单的关闭状态重置成第二套待办。

交付检查已执行：现有 `scripts/doc_consistency.py` 退出 0，输出 `document-consistency gate: OK (13 live docs, links and state agree)`；另对本目录 43 个本地链接逐项检查，无缺失。最终清单中 485 个 tracked source/build 文件 SHA-256 再核对一致。`git diff --stat` / `git diff --check` 无输出；HEAD 未变化，新增仅本审查目录，用户已有 `.trae/` 保留。新反例源码仅执行其独立 manifest 的 cargo fmt。文档检查不代表缺陷已修复，tracked diff 检查也不包含未跟踪的审查目录。

下一功能切片建议优先 R01：用户在持续修改后暂停/恢复，最新的持久状态不因旧保存确认而漏掉。验收固定同类修改发生在后台保存期间的时序，并通过正式快照内容与恢复后状态核对。随后分别处理 R02/R03 的 owner 与恢复根；平台按既有 B/C 所有权修复幂等与生命周期。每个切片独立交付，不把整个审查清零设为平台主线的新阶段。
