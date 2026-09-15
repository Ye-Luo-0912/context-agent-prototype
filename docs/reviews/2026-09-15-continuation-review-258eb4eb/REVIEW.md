# context-agent-prototype：258eb4eb 续审与当前阶段收口

审查基线：`258eb4eb1b6058258650a1bd999765650f6caf00`  
前一基线：`4aaa8bea89336e2ec0fd21c76d04967814b24020`  
日期：2026-09-15（本报告时间与日志时间均以 UTC 标注）  
范围：执行核心、Context/GC/搜索、工具、平台与供应商 KV/Prompt Cache；GUI 不扩展。

## 结论

本轮建议继续既定后端阶段，而不是新建更大的重构或审查阶段。先修正当前冷目录回归测试的确定性自锁，再补齐最终装箱的身份/覆盖一致性、冷目录的固定预算下进展、供应商缓存协议一致性，以及同任务旅程的实际进程与指令传递证据。

已经修正的全局裁剪顺序、计数随最终组装更新、无删除 GC 保留卡片记录、规范路由键，不应原样重开。新测试和文档不能替代它们所声称证明的行为。

## 1. 证据边界与 CI

这是最新变更与直接生产调用链的续审，不是全仓所有源码逐行通读。比较接口列出 12 个新增提交、29 个变更路径；变更清单不等于这些路径全部通读。代码结论来自固定 SHA 的 GitHub 原生文件读取。

本环境尝试克隆时 GitHub DNS 解析失败，未取得本地仓库；未发现 Cargo/.NET 可执行程序。因此没有在本环境执行 Rust/.NET 回归、性能测试或付费模型实验。读取了已有远端 CI 的真实状态和 Linux part 1 完整日志。没有修改、提交、推送仓库，也没有触发远端 CI。

CI run：`34917761534`，attempt 1，绑定本 SHA，最终 `cancelled`。

| Job | 结果 |
|---|---|
| fmt / clippy / build (ubuntu-latest) | success |
| fmt / clippy / build (windows-latest) | success |
| document consistency | success |
| dotnet (desktop build, client tests) | success |
| test (ubuntu-latest, part 2) | success |
| test (ubuntu-latest, part 1) | cancelled |
| test (windows-latest, part full) | cancelled |

不能将其写成全量绿色，也不能把取消写成某条断言失败。Windows job 的取消原因未在本轮独立定位。

Linux part 1（job `104219583345`）日志观察摘记（时间取到秒；以下是摘要，不是原样导出的日志文件）：

| UTC 时间（2026-09-15） | 观察 |
|---|---|
| 01:38:34 | context-simple 开始 404 项测试；`a_cold_collection_larger_than_the_hot_budget_stays_bounded_and_resumable` 随后通过 |
| 01:39:34 | `externalize_growth_demotes_the_oldest_carded_entries_back_to_pending` 被报告已运行超过 60 秒 |
| 01:40:57 | 最后可见的普通测试完成记录为 `required_store_read_serves_the_promoted_metadata_not_the_blob_snapshot ... ok` |
| 02:21:59 | 日志报告 operation canceled |

该冷目录测试在已读取日志中没有完成记录。下文 R1 的锁依赖可独立解释其不退出；这不意味着本轮证明了谁取消 CI、是否触发外层时间限制，或所有取消 job 的原因。

来源：[固定提交](https://github.com/Ye-Luo-0912/context-agent-prototype/commit/258eb4eb1b6058258650a1bd999765650f6caf00)、[比较](https://github.com/Ye-Luo-0912/context-agent-prototype/compare/4aaa8bea89336e2ec0fd21c76d04967814b24020...258eb4eb1b6058258650a1bd999765650f6caf00)、[CI](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34917761534)、[Linux part 1 日志](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34917761534/job/104219583345)。

## 2. 已观察到的修复，不重复派工

- **T1**：selected、foreground 与可选 schema 进入统一裁剪候选；最终输入与计数经 `FinalPackInputs` / 重组函数一起更新。原“先删必需 selected、后删可选 foreground”与旧计数问题不能照抄重开。R2 是新发现的契约交互边界。
- **T2**：`ExternalMap::remove_ids` 对空删除列表直接返回，部分删除保留幸存项 card claims；既有 Linux 日志中相应 no-op/部分删除测试已通过。它并不意味着所有索引维护都是 O(变更数)，删除后仍重建位置索引。
- **T3**：MCP 已增加预算化发现入口、累计限制与完整游标集合；已读取区间显示旧实现正在收敛。未在本环境重跑，不将局部阅读说成所有清理路径都完成独立行为确认。
- **T4**：有每操作 hydration 预算，也已经存在 GC 后 `demote_overflow`。不是“完全没有冷热降级”。但按 id 读取、硬时间/字节边界、正常预算下继续搜索仍未形成完整闭环。
- **T6**：`PromptCacheRouting::key_for` 已改为版本化长度前缀编码后 SHA-256，输出 `rc2-` 加 64 位 hex；旧分隔符碰撞、整体截断及工作区路径直接出现在 wire key 的问题不再按旧形状派工。
- **T7**：已存在真实 workspace/tools/checkpoint/IPC 的同任务组合测试；其复用价值应保留。R6 限定它能够证明的范围，而不是否定全部测试。

T5 的所有配置适用性及所有入口一致性，本轮未完成独立逐项验收。

## 3. R1：冷目录测试持锁递归进入引擎，确定性自锁

**优先级：立即处理的集成阻塞。类型：测试缺陷，不直接等同生产死锁。**

位置：[cold_bounds.rs:320–365](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/context-simple/src/tests/cold_bounds.rs#L320-L365)。

`externalize_growth_demotes_the_oldest_carded_entries_back_to_pending` 取得 `let state = engine.state.lock().await` 后，在同一作用域遍历被降级 id，并执行 `engine.fetch_external(*id).await`。后面仍然使用 `state`，guard 未释放。

`fetch_external` 先取得 operation gate，再通过 `hydrate_card_for` 获取 `self.state`。测试因此持有 state 并等待一个再次需要 state 的 future，无法自行退出。

### 最小修复

第一段只在锁内检查并复制待读取 id；结束作用域释放 guard。第二段通过公开引擎 API fetch，不能继续持有内部 state guard。第三段重新加锁检查结果。不要删生产锁，不要改成更宽松的所有权规则，也不要通过 ignore 或加大 CI 超时绕过。

同一测试还有一个被死锁遮住的计数错误：初始 40 条，随后 `externalize_n(..., 3)` 创建 3 条新 owner，末尾却仍把 `external.len() + pending.len()` 与 40 比较。在没有删除的本场景下应为 43。更稳妥的是对“旧 ids ∪ fresh ids”进行集合比较，并断言 hot 与 pending 的交集为空；只核对总数量不足以排除丢一条又重复一条。

给目标测试加局部超时，使死锁尽快表现为明确失败，而不是消耗整个 CI 时限。超时用来检测，不用来宣称完成了清理。

生产路径依据：[engine.rs:2680–2820](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/context-simple/src/engine.rs#L2680-L2820)、[hydrate_card_for](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/context-simple/src/engine.rs#L1380-L1430)。

## 4. R2：正文被另一 ID 覆盖时，最终必需 ID 清单仍可能不一致

**优先级：P2 执行正确性。证据：静态控制流反例；未本地执行。**

位置：[actor/model.rs:110–225](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/agent-runtime/src/actor/model.rs#L110-L225)、[最终裁剪调用方](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/agent-runtime/src/actor/model.rs#L1010-L1395)、[context.rs 精确 ID 校验](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/agent-contracts/src/context.rs#L2300-L2345)。

`record_final_pack_drop` 允许另一条记录凭相同 path/revision 与完整范围覆盖被删除正文；覆盖成立就返回 false，不添加 miss。调用方仅在返回 true 时从 `required_item_ids` 移除该 ID。最终 validator 却要求每个仍列入的 required ID 都精确出现在 items/foreground 中。

反例：两条不同 ID 的必需记录 A/B 覆盖同一版本同一范围，预算只容纳一条。删除 A 后 B 仍覆盖证据，helper 不记 miss，但 A 仍留在 required IDs。最终验证报告 A 缺失并中止准备，虽然所需正文仍在帧内。

这是合法 MaterializedContext 组合可以触发的边界，并不宣称 SimpleContext 的每次普通 fs.read 都产生此组合。

### 最小修复与验收

分开处理“具体记录是否仍存在”与“证据义务是否被其他记录满足”。剩余物理 ID 集合必须与最终帧一致；覆盖映射决定是否添加 miss以及何种来源仍承担必需义务，不能由“是否刚添加了一个 miss”的 bool 顺带决定身份清单。

不要简单清空 required IDs 或放宽 validator。回归至少涵盖：同 ID 双层投影删一份、异 ID 同版本同范围覆盖、异版本不得覆盖、相邻区间与未覆盖区间、真正预算不足时如实产生 miss。测试必须走最终裁剪和最终校验，而不是只测 helper。

## 5. R3：T4 的“有界”仍是局部控制，未成为全操作后置条件

**类型：T4 主体能力未收口；不是把第一期全部判废。**

位置：[hydrate_within_budget / per-id 路径](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/context-simple/src/engine.rs#L1240-L1430)、[GC 后降级](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/context-simple/src/engine.rs#L2150-L2230)、[demote_overflow](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/context-simple/src/index/external.rs#L289-L365)。

### 时间边界

绝对 deadline 只在批次开始前检查；`hydrate_pending_cards(take).await` 内的读取没有使用该剩余 deadline。一个已开始但迟迟不完成的读取可以越过预算。当前可准确称为“批次之间停止继续工作”，不能据此声称全操作硬截止。

建议把同一剩余 deadline 传递到可取消读取等待。取消/超时保持 pending owner，不提交未完成安装。这里约束的是调用等待与状态结算；不能承诺底层系统 I/O 已被物理取消。

### 字节边界

`take` 由条数剩余额度和 entry room 决定，不根据当前剩余 metadata bytes 预留空间。在字节上限前只剩少量余量时，一批较大元数据仍能安装后越界。检查时使用的是 metadata 估算，也不等同实际 RSS。

应明确估算口径，并在安装前按条目或保守上界预留；超额条目仍保留可寻址冷 owner，不能为了守 cap 丢元数据。

### 全入口边界与进展

per-id fetch/inspect 明确不走 bulk hot cap。连续读取不同 id 可以继续扩大热表，而 `demote_overflow` 只在一次 full GC 的提交后调用；它还跳过 pinned 与无有效 card claim 的条目。无合格可降级项时，函数直接停下，没有把“无法恢复到预算内”作为独立返回结果。

保留必需证据比假守 cap 更重要，但超限必须有明确的例外/背压状态。不要对每次读取另建一套驱逐策略，建议把安装、读后驻留、降级与预算报告集中到同一 metadata-residency 入口。固定配置下访问 N 倍于热预算的历史后，热条数/估算字节仍应受控或明确背压；不靠不断提高配置上限才能继续。

现有测试通过“提高热上限后排空全历史”证明可取回性，但它没有证明固定预算下完整搜索能力。pending locator 目录本身仍然随历史增长；热 metadata 有界不等于全部历史管理内存恒定。

## 6. R4：非空搜索结果丢掉 coverage，正常预算下无法表达完整续查

**优先级：P2，T4 正确性与搜索能力。证据：引擎和 Core 返回链已静态核对。**

位置：[finish_search_hits](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/context-simple/src/engine.rs#L1320-L1365)、[Core 模型输出](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/agent-core/src/kernel/mod.rs#L580-L715)。

目前 `hydration.complete || !hits.is_empty()` 就返回普通 `Vec`。Core 只在 hits 达到请求 limit 时显示 result capped。1 个命中、limit=20、仍有未读冷页时，模型收到普通结果，没有候选覆盖缺口，也没有继续查询的边界。

“排名结果只返回 Top-K”与“有一部分候选根本没有检查”是两种事实。即使普通搜索不保证穷举，也不能靠这个理由抹去已知的未读范围。零命中已有 fail-closed 应保留；非空结果应保留有用 hits并附上完整性，而不是一律拒绝。

建议沿既有 ContextEngine/service/Core 输出协议传递统一结果：hits、coverage、remaining、stop_reason，以及必要时绑定查询和目录 revision 的 continuation。显示给模型的正文也必须承载不完整事实，不能只记 diagnostics/metadata。内部已有 HydrationOutcome 应复用，不再用字符串解析作为状态契约。

在固定热预算下，续查应推进到后续冷页；不能同一批 hot entries 满员后反复 HotCap，也不能让一个坏页永久挡住后续可读页。GC 在引用关系仍未完成时继续保守延期，但需要可收敛的扫描/引用进度，而不是永久要求全量元数据同时驻留。

## 7. R5：供应商 KV 仍在固定已知的错误编码，免费一致性检查不应等待付费实验

**优先级：P2，适用于声明支持该官方显式缓存协议的 profile。类型：协议一致性缺口；未做真实请求。**

位置：[真实 mapper](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/provider-openai/src/lib.rs#L974-L1125)、[新增 shape fixtures](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/provider-openai/src/prompt_cache/endpoint_shape_tests.rs)、[已修路由键](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/agent-contracts/src/model_cache.rs)。

规范路由 key 已修。但 mapper 对声明式多断点仍保留 string content，然后把 breakpoint 放到 input item sibling；旧单断点分支反而会放在 content block 中。

新增测试明确断言 mapper 继续与官方形状不同，并称等待 T8 真实端点确认后再修改。这是现状刻画，不是协议通过。官方文档将 Responses 断点放在受支持的 input_text/input_image/input_file 内容块，并说明不支持的块会被拒绝。当前未向用户配置的实际端点发请求，不能把文档推导说成已实测 400。

### 建议

官方协议 profile 按已公开的协议修改 mapping，并让本地 fixture 校验正确性。兼容网关若确有另一个已确认方言，应分开命名和测试，不能因为一个历史捕获测试固定了 sibling 字段就默认继续通用。未知能力端点继续不发送专属字段。

模型输出之后不能任意改断点索引：最终已验证的缓存计划应同时绑定最终消息/工具与所声明的稳定边界。字符串、空消息过滤、tool call展开都属于 mapper 的映射职责，不应改变缓存边界语义。

T8 只承担真实端点接受、真实命中、重写和净任务费用的条件实验；不承担解除已知本地协议缺陷的责任。

官方依据：[OpenAI Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)。仅用于缓存协议比较，不把某一家规则推广为所有供应商规则。

## 8. R6：T7 是有价值的组合恢复测试，但尚未证明新进程和纠正指令的因果链

**类型：验收覆盖缺口，不直接证明生产恢复功能坏了。**

位置：[host_t7_journey.rs（已通读）](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/258eb4eb1b6058258650a1bd999765650f6caf00/crates/agent-host/tests/host_t7_journey.rs)。

测试确实运行了真实工作区写入、真实审批、平台 wire、checkpoint恢复，以及跨会话不重放既有写入。第二个场景还覆盖恢复后外部条件导致的写入失败与重试。这些覆盖应保留。

但测试的 server 使用 `std::thread::spawn`；session-1 shutdown 后在同一个测试进程内重新 compose。新 RunId不是新 OS 进程。它没有单凭这条旅程验证宿主进程死亡后全局状态、句柄、watchdog 和锁重新初始化。

同时，脚本模型固定生成 `Q:` 前缀，没有根据 steer 文本决定结果；恢复后的脚本模型由测试以 `JourneyModel::new(6, ...)` 直接给定下一阶段。顺序控制本身合理，但没有配套断言时，无法证明纠正正文已经进入下一次请求、恢复后的模型从任务状态取得了正确剩余义务。

该 fixture 采用 Rolling、无模型压缩器、无 cache routing；不应扩展声称它同时证明 Dynamic 冷目录或供应商 KV 的长流程性质。

### 最小补充

保留现在的快速组合测试，并准确标注为同进程重组。用已有 host 二进制与本地脚本 HTTP provider 增加独立进程变体：记录不同 PID，确认前一进程已退出，从磁盘恢复同一 TaskId/lineage，再继续。无需真实付费模型。

纠正输入使用一次性唯一标记；脚本 provider 必须检查收到的 request 确实包含该标记与正确任务约束，缺失就拒绝推进。恢复后的请求也应包含正确剩余任务事实。这样才可证明运行时把信息交给模型，而不是测试脚本预先知道目标答案。

若该旅程承担 T4/T6 验收，再单独加入超过窗口的证据回取与本地 wire 捕获，不能仅从一般 file write/restore 推出这些性质。

## 9. 可维护性的收敛方向

本阶段不要求全仓拆文件或另造框架。应收敛四个实际导致本轮问题的责任：

| 边界 | 单一负责者应产出的事实 | 不应继续依赖 |
|---|---|---|
| 最终装箱 | 最终帧、可见 ID、证据覆盖、计数、工具快照和缓存边界 | 一个 bool 同时代表有无 miss和身份是否保留 |
| 冷目录 | 原子 owner迁移、预算后置条件、coverage、可推进游标 | bulk/read/GC 各自执行不一致的 cap |
| 缓存协议 | 由最终请求验证得到的计划和方言映射 | 裸索引、互相矛盾的 legacy/new分支、固定错误 wire 的测试 |
| 验收回执 | 对应 SHA 下实际执行的行为及明确限制 | 新 RunId代替新进程、预设结果代替指令传递、枚举类型最后变字符串 |

注释应描述当前不变量，而非不断附加 T4/B2/Nxx 的历史补丁说明。例如 `hydrate_all_pending_cards` 已不再保证读完所有 pending，应改名或改成明确的预算化契约，历史理由转到相应回执。命名调整随功能修改完成，不单独开启大规模整理任务。

## 10. 直接执行的收口顺序

| 顺序 | 工作切片 | 完成条件 |
|---|---|---|
| S1 | 修复 cold_bounds 自锁与 owner 集合断言 | 目标回归能在局部期限内结束，不 ignore、不放宽生产锁；本 SHA/后续修复 SHA 的既有 CI 完成 |
| S2（可并行） | 最终装箱 ID/覆盖一致性 + 官方缓存 mapping | 异 ID 等价证据不会产生结构性失败；真实mapper输出满足对应profile的正确schema |
| S3 | 固定预算下冷目录读/写/降级/续查闭环 | 多倍热预算历史可查询、热资源受控或明确背压；部分结果携带coverage；删除不依赖未知引用 |
| S4 | 复用T7补充进程与指令传递证据 | 不同PID、同任务恢复，唯一纠正标记在下一请求与恢复请求可证；结果不靠脚本提前知道 |

付费T8仍条件化，GUI仍后置。以上动作可以直接归入当前NEXT_TASKS，不要将历史已关闭条目整份重新追加，更不要新增一轮“审查清零”前置阶段。

### 定向命令（提供给实现者；本环境未运行）

使用仓库已声明的工具链和既有CI环境；仅执行不依赖真实模型的测试：

```bash
# 先修 S1，再运行；修复前该目标会自锁，应由测试内部期限及时检出。
cargo test -p context-simple externalize_growth_demotes_the_oldest_carded_entries_back_to_pending -- --nocapture --test-threads=1
cargo test -p context-simple cold_bounds -- --nocapture --test-threads=1

cargo test -p agent-runtime final_pack
cargo test -p provider-openai endpoint_shape_tests
cargo test -p agent-compose --test cache_routing_wire_acceptance
cargo test -p agent-compose --test cache_wire_flow
cargo test -p agent-host --test host_t7_journey

# 定向通过后按既有 CI，而不是新增重复评测框架。
```

新反例测试名应由实现者按现有布局加入，上述命令不意味着尚未编写的回归已经存在。记录实际命令、修复 SHA、平台、是否执行及结果；编译通过不能代替新反例通过。

## 11. 本轮读取覆盖

此表列的是用于本轮结论的实际读取范围，不将文件枚举或搜索命中当作全文审查。

| 文件 | 本轮读取 |
|---|---|
| docs/CURRENT.md | 当前状态正文 |
| docs/NEXT_TASKS.md | 当前 T1–T8、关闭声明和限制相关正文 |
| crates/context-simple/src/tests/cold_bounds.rs | 全文，含补读尾部 |
| crates/context-simple/src/engine.rs | hydration/单条读取相关区间；2080–2300 的GC/reconcile；2680–2820 的fetch/checkpoint |
| crates/context-simple/src/index/external.rs | 230–465：删除、降级、索引与card mutation |
| crates/agent-runtime/src/actor/model.rs | 1–247：统一候选/覆盖；1010–1395：最终装箱与校验、计数 |
| crates/agent-contracts/src/context.rs | 多个区间，重点 materialization/coverage/required ID验证；不是全文 |
| crates/agent-core/src/kernel/mod.rs | 580–715：search到模型输出 |
| crates/agent-capability-process/src/mcp.rs | 130–380：reap与预算化发现入口，非全文 |
| crates/provider-openai/src/prompt_cache/endpoint_shape_tests.rs | 全文；另复核1–115文档与fixture |
| crates/provider-openai/src/lib.rs | 960–1140：真实Responses mapper |
| crates/agent-contracts/src/model_cache.rs | 全文 |
| crates/agent-host/tests/host_t7_journey.rs | 全文1–1363，分段读取 |

另读取了主线元数据、比较清单、CI run/job状态、Linux part1日志和官方Prompt caching文档。未完整读取其余源码、其余测试、全部变更diff或所有配置入口，因此不宣称“全仓逐行审查完成”。
