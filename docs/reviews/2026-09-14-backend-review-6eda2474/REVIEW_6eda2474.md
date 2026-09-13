# 全仓范围续审：长流程后端、恢复安全与供应商 KV 接线

审查版本：`6eda247401092a18bcbd547e475bc79b1ed3a4c1`  
仓库：`Ye-Luo-0912/context-agent-prototype`  
记录日期：2026-09-14（Asia/Tokyo）  
对照版本：`489c89cd7d139f4e721348ec9c54054700d93b7b`

## 结论

本轮最高优先级是 **恢复元数据被错误删除** 与 **取消期间丢失 pending 所有权**。供应商 KV 的 key 和多断点声明也没有完成到真实 HTTP 请求的接线。维持“后端主体优先，GUI 维护模式”，不重做 RuntimeActor、TaskManager 或 ContextEngine，不建设第二份任务真相。

## 证据等级与范围

本报告是**全仓范围的静态续审，不是所有代码已逐行通读的证明**。已核对20个 workspace crate 和根目录；源文件读取范围、入口级核对及未读项见 `COVERAGE_6eda2474.md`。测试、SDK、GUI、脚本及多份生产文件仍未全部读取。不能给出可审计的“全仓阅读率”，也不能用目录/文件名覆盖替代源码覆盖。

本地 GitHub DNS 解析失败；工作环境没有找到 cargo/dotnet。没有在本地执行 Rust/.NET 测试，没有调用付费模型，没有修改/提交/推送用户仓库。下列反例是从控制流推导的**待执行回归**，不是声称已经在运行中复现。

固定 SHA 的远端 CI run [34766940848](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34766940848) 已报告 `completed/success`。这只证明该版本的现有 CI 通过，不能证明下列未被覆盖的路径正确。上轮489版本的失败不得继续作为6eda版本失败引用。

## 问题索引

这些编号仅用于本报告定位，落地时映射到已有开放工单，不建立永久平行队列。

| 编号 | 建议级别 | 问题 |
|---|---|---|
| N01 | P1 | 元数据卡片清理绕过保护根完整性，服务冷启动可破坏保留 checkpoint |
| N02 | P1 | 冷页读取先移除待加载 owner，再 await；取消与临时 I/O 失败会丢失目录项 |
| N03 | P2 | 卡片读入缺少硬字节上限与捕获校验和验证，延后页结构验证不完整 |
| N04 | P2 | 供应商 KV 路由键与多断点停在声明层，未完整进入生产 HTTP 请求 |
| N05 | P2 | 空选中集回退到全 context 前缀，且用正文字符串猜 EvidenceSplit |
| N06 | P2 | 公开 EvidenceSplit 数量未经验证就参与相加与切片 |
| N07 | P2 | process.session 把信号退出视为 running，退出结果未完整到达模型正文 |
| N08 | P2 | process.session poll 的本地 drain 没有总工作界限，且持有整个 registry 锁 |
| N09 | P2 | shell/process 的退出后排空 grace 从启动阶段开始计时 |
| N10 | P2 | MCP 工具发现忽略 nextCursor，首批被当成完整静态 manifest |

## N01 · P1 · 元数据卡片清理绕过保护根完整性，服务冷启动可破坏保留 checkpoint

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/context-simple/src/store.rs:1450–1765](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/store.rs#L1450-L1765)；[crates/context-simple/src/engine.rs:1740–1945](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/engine.rs#L1740-L1945)；[crates/agent-context-service/src/main.rs:1–120](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-context-service/src/main.rs#L1-L120)；[crates/agent-context-service/src/lib.rs:1–155](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-context-service/src/lib.rs#L1-L155)

**观察：** blob 清理分支使用 roots_complete，但 cards 目录的清理只检查 map_checksums 和 protected 是否包含 id。它不检查 roots_complete，也没有把本次 blob 扫描待重新认领的 rebuilt_candidates 纳入保护集。服务启动时 build_engine 创建新引擎，然后在接收 Restore 前调用普通 reconcile_store；普通版本传入空保护集和 roots_complete=true。

**触发条件：** 路径一：调用保护性 reconcile，但根枚举失败，传 roots_complete=false；不在当前 map/已知 protected 中的卡片仍可能被删除。路径二：已有包含 external_spilled 的保留检查点，关闭服务，再在同一持久 store 上启动新服务；空 map 下的 blob 被排入待认领列表，卡片却在这些 owner 提交前被当孤儿删除。

**后果及边界：** 恢复所依赖的 .card 元数据可能被实际删除，后续恢复只能缺失或降级。不能把这个结论扩大为所有 .json 原文立即被删除：原文可能还在，失去的是 checkpoint 捕获的元数据版本及恢复承诺。

**最小修复：** 统一 blob 与 card 的删除许可：根集合不完整时延后所有相应删除。恢复所有权尚未安装的启动阶段采用非破坏性 reconcile；仅把启动参数换成 false 不够，必须先修 cards 分支。当前 pending owner 与本轮待认领 owner 必须计入保留判断。继续复用现有根枚举、store 和事务，不增加第二个根数据库。

**必要回归：**
- 真实 service 进程：外置 → 分片 checkpoint → 退出 → 同 store 重启 → restore → 原文与捕获元数据逐项一致。
- roots_complete=false 且没有已知 owner 的卡片不得删除；故障消失后完整枚举才允许做删除判断。
- 同一次 reconcile 的有效 ownerless blob 与对应 card 一起保留，随后只产生一个 owner。

**停止条件：** 两类删除反例都变为保留；完整根条件下真正孤儿仍能清理；原文、卡片与检查点恢复语义一致。

## N02 · P1 · 冷页读取先移除待加载 owner，再 await；取消与临时 I/O 失败会丢失目录项

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/context-simple/src/engine.rs:990–1155](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/engine.rs#L990-L1155)；[crates/context-simple/src/engine.rs:1740–1945](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/engine.rs#L1740-L1945)；[crates/context-simple/src/engine.rs:2090–2285](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/engine.rs#L2090-L2285)

**观察：** hydrate_pending_cards 先 split_off/mem::replace 移出 pending_external_cards，再逐卡 await；hydrate_card_for 先 remove(position)，再 await。没有保留状态内 owner 的 in-flight 记录或取消归还守卫。读取的所有失败被合并为 missing 并消费该记录。

**触发条件：** 在页已从 pending 集合移出、但磁盘读取尚未返回时取消/dropping future。另一个触发是权限、设备或短暂读取错误；即使稍后磁盘恢复，这个 pending locator 也已被消费。

**后果及边界：** 条目可从当前逻辑目录和下一份 checkpoint 的 manifest 消失。op_gate 只保证互斥，不自动实现取消回滚。部分上层回滚可能补救，但底层不能依赖每个调用方恰好回滚；是否进一步引发错误清理由对应集成用例验证。

**最小修复：** 在状态中保留原 pending owner，锁内只复制有限读取计划；锁外读取；重新拿锁后仅对验证成功的相同 owner 提交迁移。或者使用可证明的取消安全所有权守卫。对 Missing、Corrupt、IoFailed、Cancelled 分别处理，临时失败保留可重试位置，不按“没有数据”消费。

**必要回归：**
- 门控 card read：取计划后挂起，abort，再比较 owner 集、diagnostics、下一份 checkpoint manifest。
- 单 id fetch 与整批 hydration 分别取消；覆盖成功一部分、下一张卡等待中的情形。
- 注入一次临时 I/O 失败，恢复磁盘后同 id 仍可读取且无双 owner。

**停止条件：** 任何 await 边界取消都不丢 owner；可重试失败不永久丢引用；成功迁移恰有一个 owner。

## N03 · P2 · 卡片读入缺少硬字节上限与捕获校验和验证，延后页结构验证不完整

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/context-simple/src/store.rs:1–295](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/store.rs#L1-L295)；[crates/context-simple/src/engine.rs:990–1100](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/engine.rs#L990-L1100)；[crates/context-simple/src/engine.rs:2420–2560](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/engine.rs#L2420-L2560)

**观察：** read_external_card_async 使用 tokio::fs::read 整体分配；parse_external_card 只验证 envelope schema 和 item_id，不接收 manifest 中的 expected hash。初始 restore 会校验候选状态，但延后页直接 merge_paged，未经过等价的完整结构校验。正式 .json blob 已有有界读取，可以复用。

**触发条件：** 私有 store 中卡片被异常替换、损坏为超大文件，或者同 id 内容与 checkpoint 捕获的 hash 不一致；错误 scope 等字段恰好位于启动首批以后的页。

**后果及边界：** 可能产生不必要的大分配，或把不属于该捕获版本的元数据装入运行状态。本轮没有证明外部未授权攻击入口，不应标为已确认远程利用。当前 FNV 校验和不是密码学认证，修复也不能把它宣传成强认证。

**最小修复：** 复用 opened-handle + metadata 检查 + take(cap+1) 的有界读模式。按已有格式校验 expected checksum/prefix；需要改为强摘要时显式做格式迁移，不暗改旧检查点。延后页入库前验证 id、owner、scope 和必要结构关系；非法页不改变运行状态，并保留可诊断定位。

**必要回归：**
- 超限 card 在有界读取处拒绝，而不是先全量分配。
- 同 id/不同捕获字节被判为不匹配。
- 首批之后的卡片引用不存在 scope，读取不得静默安装。

**停止条件：** 卡片与正文读路径拥有同等级的资源和一致性边界；合法旧卡仍能读取。

## N04 · P2 · 供应商 KV 路由键与多断点停在声明层，未完整进入生产 HTTP 请求

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/agent-contracts/src/model.rs:398–630](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-contracts/src/model.rs#L398-L630)；[crates/provider-openai/src/lib.rs:960–1125](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/provider-openai/src/lib.rs#L960-L1125)；[crates/agent-runtime/src/actor/model.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-runtime/src/actor/model.rs)；[docs/reviews/2026-09-13-kv-cache-layout-489c89cd/C_LINE_PROVIDER_RECEIPT.md](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/docs/reviews/2026-09-13-kv-cache-layout-489c89cd/C_LINE_PROVIDER_RECEIPT.md)

**观察：** ModelInput::into_request 生成 cache_breakpoints 列表，却仍默认 prompt_cache_key=None。实际 actor/model 生产调用直接把 into_request 的结果交给 complete_stream，未填写 key。Responses mapper 仍从旧 prompt_reuse_boundary 得到唯一 boundary_index，忽略新的 cache_breakpoints 列表；虽然已会转发非空 key，但生产调用方没有提供。C 线回执也仍留有“谁填 key”的接口请求。

**触发条件：** 运行真实 Compose → RuntimeActor → Provider 请求，而不是手工构造带 key/breakpoints 的测试 ModelRequest。证据变化后希望复用较早 B0 时，wire 实际未声明这个独立边界。

**后果及边界：** 已声明的 B0/B1 分层与稳定路由键收益没有端到端实现。不能推断“所有供应商缓存完全失效”：自动缓存或唯一旧边界仍可能命中。缺陷是意图中的接线未完成，不是服务端命中率已被实测为零。

**最小修复：** 在现有 runtime/组合根的最终请求构建点派生稳定、不透明、按真实隔离域限定的 key。不要每轮生成 UUID，也不要用完整动态请求 digest 充当稳定路由键。mapper 对可支持的多个合法边界逐一校验并映射，保留空消息删除及协议展开后的索引对应关系。不同端点的能力不能从模型别名/兼容 URL 猜测。

**必要回归：**
- 捕获由真正组合链路发出的 HTTP：同任务两次请求 key 相同，策略末端 B0 与证据末端 B1 均实际可见。
- 仅证据变化时 B0 内容不变，当前指令仍更新；工具撤销必须真实反映在请求中。
- 不同有效隔离域不共享不应共享的 key；未知能力端点不接收专属字段。

**停止条件：** 生产 wire 而非仅 DTO/assertion 证明 key 与多断点存在；不在此片宣称金额收益。

## N05 · P2 · 空选中集回退到全 context 前缀，且用正文字符串猜 EvidenceSplit

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/agent-runtime/src/prompt.rs:315–510](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-runtime/src/prompt.rs#L315-L510)；[crates/agent-contracts/src/model.rs:398–630](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-contracts/src/model.rs#L398-L630)

**观察：** assembler 用 content.contains("SELECTED WORKING CONTEXT") 判断是否存在稳定证据，然后设置 Some{base:0,epoch:1}；没有命中则 None。into_request 把 None 解释为旧布局兼容：把整个 context_frame 纳入可复用前缀。

**触发条件：** 当前没有 history.items，但有 foreground、required misses、external refs 或恢复投影。另一反例：文件正文恰好含有该标题字符串，误触发布局判断。

**后果及边界：** 新请求的空稳定集被混同于旧格式，易变尾部重新进入缓存前缀；来源文本可以影响布局声明。这是缓存分段错误，不是本轮已证明的权限提升。

**最小修复：** 按构建时的类型化消息计数设置 split；当前新 assembler 即使无稳定证据，也明确声明 Some{base:0,epoch:0}。None 只保留给确实缺少新声明的旧序列化输入，绝不扫描正文决定结构。

**必要回归：**
- selected 为空，分别改变 foreground/misses/restored，稳定 policy 边界保持不变。
- 文件正文含完整标题字面量，不改变其所属分区。
- 旧序列化输入按明确兼容路径解析，新输入不进入旧分支。

**停止条件：** 空集与旧格式可区分；内容不能决定结构；前缀计数来自真实组装。

## N06 · P2 · 公开 EvidenceSplit 数量未经验证就参与相加与切片

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/agent-contracts/src/model.rs:398–630](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-contracts/src/model.rs#L398-L630)

**观察：** base/epoch 是可反序列化的 usize 字段。into_request 直接相加 system_policy.len()+base+epoch 并取 messages[..declared_len]，未验证 base+epoch 不超过 context_frame.len()，也未用 checked_add。

**触发条件：** 库调用方或反序列化输入提供超限 split；较小但错误的数量也可能落入扁平 messages 的 turn 区域。当前生产 assembler 只生成小计数，所以这不是已证明的正常用户触发崩溃。

**后果及边界：** 不合法的结构提示可变为 panic/溢出或错误边界，而不是类型化拒绝。这里的严重性是公共契约健壮性；尚未证明网络可达性。

**最小修复：** 在 flatten 前对所属层验证数量并使用 checked arithmetic。异常声明应通过 Result 拒绝，或明确放弃该缓存 hint 而完整保留原消息；不静默扩展到 turn 内容。继续绑定真实最终消息与工具，不能只相信整数位置。

**必要回归：**
- usize::MAX、超过 context_frame.len()、恰好到边界及空消息组合。
- 无效声明不 panic，不改变请求正文/角色，不把动态区缓存声明为稳定区。

**停止条件：** 结构校验先于索引运算；有效兼容输入行为不变。

## N07 · P2 · process.session 把信号退出视为 running，退出结果未完整到达模型正文

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/tool-runtime/src/tools/session.rs:1–145](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/session.rs#L1-L145)；[crates/tool-runtime/src/tools/session.rs:567–686](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/session.rs#L567-L686)；[crates/agent-contracts/src/model.rs:270–335](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-contracts/src/model.rs#L270-L335)

**观察：** drain 返回 Option<i32>，已退出时取 status.code()；poll 用 Some(code) 区分 exited，用 None 表示 running。Unix 信号终止的 code() 本来就是 None。正常退出码只写在 metadata；model_content 的 status 文本未区分 exit=0 与 exit=7。TurnFrame.messages 只转发 model_content。

**触发条件：** session 进程由信号结束；或两个任务输出相同/为空，但一个退出0、另一个退出7。

**后果及边界：** 模型可能继续轮询已经结束的进程，或缺少“任务成功还是失败”的直接结果信息。poll API 成功不等于被轮询进程成功，这两类事实应分别呈现。

**最小修复：** 用明确退出状态保存 Running / Exited{code,signal,success}，输出收集完整性另列，不能借 running 表达“已退出但管道未读完”。把模型决策必需的终止结果放入有界 model_content；metadata 保留精确事实。

**必要回归：**
- exit0、exit7、signal、先 EOF 后退出、先退出后 EOF。
- 经过实际 dispatcher → broker → TurnFrame，确认模型看到退出结果，而非仅检查 metadata。

**停止条件：** 每种真实进程终态有明确表达；终态不会永久显示 running；模型可区分成功与失败。

## N08 · P2 · process.session poll 的本地 drain 没有总工作界限，且持有整个 registry 锁

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/tool-runtime/src/tools/session.rs:1–145](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/session.rs#L1-L145)；[crates/tool-runtime/src/tools/session.rs:567–686](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/session.rs#L567-L686)

**观察：** 初段 try_recv 循环没有总 chunks/bytes/deadline/cancel 检查，并在 capture.record 的 await 期间允许生产者补入数据。退出后每次 recv 都重新计一个1秒超时，持续产生数据的后代可以不断延后结束。poll 持有共享 sessions mutex 等待 drain 与 artifact.flush。

**触发条件：** 持续输出进程、持有管道且持续输出的后代，或者慢 artifact 写盘；同时停止另一个 session。

**后果及边界：** 单次 poll 的本地耗时没有宣称的总界限，并可拖住其他 session 的控制操作。外层 runtime 超时可能最终中止它，所以不能把静态结论夸大为整个宿主一定永久死锁。

**最小修复：** registry 只用于短时间获取 session handle，长 I/O 使用每会话状态保护。设置绝对批次截止点、最大 chunks/bytes，并在循环中检查 cancel；没有读完的输出留待下一批，正确报告 output_pending。

**必要回归：**
- 连续 writer 下 poll 仍在配置总预算内返回。
- poll A 的同时 stop B 不被 A 的磁盘等待拖住。
- 小于1秒周期的后代输出不导致退出 drain 无限延长。

**停止条件：** 每次 poll 有可证明的总工作边界；控制路径不被其他 session 长 I/O 占住；剩余输出不丢。

## N09 · P2 · shell/process 的退出后排空 grace 从启动阶段开始计时

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/tool-runtime/src/tools/shell.rs:235–500](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/shell.rs#L235-L500)；[crates/tool-runtime/src/tools/process.rs:970–1240](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/process.rs#L970-L1240)

**观察：** grace=sleep(500ms) 在执行循环前建立，child.wait 返回时只把 grace_started 设为 true，没有重置 deadline。运行超过500ms的命令，启用分支时该 timer 已过期。

**触发条件：** 运行超过500ms，退出时尚有未送达消费端的 pipe 尾部；select 可以先命中过期 timer 而结束采集。

**后果及边界：** 最终日志或测试尾部可能丢失。现有 artifact_truncated 是容量裁剪事实，不能代替“管道是否排空”；仅因未触发大小 cap 就写 Full output 会过度宣称完整性。不是每次运行都必然触发，需门控时序回归。

**最小修复：** 在首次观察到进程退出时建立/重置 grace 截止点；区分 process_exited、pipes_drained 和 size_truncated。未排空就诚实报告，不把短前缀当完整输出。

**必要回归：**
- 门控 reader：子进程运行>500ms后退出，最终 sentinel 在退出后 grace 内抵达，必须收集到。
- 管道被后代持有超过 grace 时有界结束并显式报告不完整。

**停止条件：** 排空预算从退出时刻计算；容量裁剪与未排空两种不完整可区分。

## N10 · P2 · MCP 工具发现忽略 nextCursor，首批被当成完整静态 manifest

**状态：源码路径已核对；运行反例尚未在本地执行。**

**源码定位：** [crates/agent-capability-process/src/mcp.rs:1–330](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-capability-process/src/mcp.rs#L1-L330)；[crates/agent-capability-process/src/mcp.rs:520–850](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-capability-process/src/mcp.rs#L520-L850)

**观察：** list_tools_with_cancel 只发送一次 tools/list，读取 tools 数组后返回，未处理 nextCursor。adapter connect 随后从这份结果建立静态工具清单，并关闭发现连接。仓库钉住的2024-11-05协议已经支持分页。

**触发条件：** 合法 MCP 服务把必要工具放在 tools/list 第二页或后续页。

**后果及边界：** 后续页工具不会进入能力目录，模型可能无法发现实际存在的能力。这不是版本升级需求，而是现有协议支持范围的完整性缺口。

**最小修复：** 在既有客户端补有界发现循环：最大页数、总字节、总工具数和整体期限；处理取消、重复 cursor、重复 tool 名及中途失败。达到边界时返回明确不完整/拒绝安装，或使用明确分页目录契约，不能静默安装“看似完整”的首批。

**必要回归：**
- 两页 server，只有第二页提供目标工具，正式 adapter 可以发现并调用。
- 重复 cursor、有界超限、第二页失败和取消，均不静默当作完整发现。

**停止条件：** 按既有协议覆盖分页，所有退出都说明发现是否完成，不引入新能力平台。

## 已修复项与排除项

- `search.grep.scan_continuation` 已出现在新 schema 中；不再重开上轮“后端支持而模型参数未声明”的问题。这里只确认代码表面，不声称本地执行了新回归。来源：[crates/tool-runtime/src/tools/search.rs:370–430](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/search.rs#L370-L430)。
- 新 assembler 已把 selected evidence 放在 foreground 之前，并有动态投影分离和 EvidenceSplit 声明；N04/N05 是剩余接线/空集问题，不是否定已有修复。来源：[crates/agent-runtime/src/prompt.rs:315–510](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-runtime/src/prompt.rs#L315-L510)。
- 新重试层已有 KnownAttemptUsage，对已知失败尝试和成功输出的聚合不能继续使用上轮“只记最近失败且成功丢历史”的旧描述。取消时 CallStage 与 runtime/journal 账目的最后接线仍需独立验证；不把可选 observer 的存在当计费完整性证明。来源：[crates/provider-openai/src/retry.rs:213–420](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/provider-openai/src/retry.rs#L213-L420)、[crates/provider-openai/src/retry.rs:625–845](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/provider-openai/src/retry.rs#L625-L845)。
- 正式 work.restore 没有被短 work-control 请求 timeout 直接截断；该怀疑已排除，不作为缺陷。来源：[crates/agent-runtime/src/platform/work.rs:650–970](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-runtime/src/platform/work.rs#L650-L970)。
- AppendOnly 的历史增长属于实验 baseline 的已声明语义，不按普通产品内存泄漏报告；默认 Rolling 的选择是产品配置决策，不建议在这次修复中顺手改为 Dynamic。

## 规模与配置残余：不混入已证明运行故障

### 分批读不等于一次操作有界

`hydrate_all_pending_cards` 循环到 pending 为空，search、GC、storage GC 和 reconcile 会调用它。每批256一类的设置，不等于一次查询只读256张，也不等于历史元数据最终不再全量驻留。当前可先明确支持的历史规模；下一片在现有接口上增加每操作预算、续体和完整性状态。不能为了快速搜索跳过未读 owner 后，又允许 storage GC 把它们当孤儿。

### 零模型压缩预算与占位折叠需要明确区分

组合根在压缩预算不允许调用时不注入 compactor。Rolling 在 compactor=None 下仍会用 bounded fallback marker 折叠，并且调用/Token预算判断只包围 Some(compactor) 分支。这符合“零付费调用”，不自动等于“保留全部源信息、不做折叠”。需要明确生产 profile 是否允许测试/基线式占位压缩；不能让操作员以为关闭付费压缩就一定保持源内容。这里是产品语义决定，不夸大为所有正常调用都会丢数据。

来源：[crates/agent-compose/src/lib.rs:85–165](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-compose/src/lib.rs#L85-L165)、[crates/context-baselines/src/rolling.rs:345–900](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-baselines/src/rolling.rs#L345-L900)。

### 宿主死亡监督的条件性降级

watchdog 已增加可重试 spawn 失败的恢复，不应继续把旧 CI 故障当未修。仍需明确：在耗尽重试后选择仅日志提示并继续执行，是否符合该 lane 声明的宿主死亡清理保证。要求严格清理的 lane 应中止并清理尚未交付的进程，或者明确标识较弱 profile；不应以“监督句柄未建立”却仍宣称完整保护。不是本轮已执行复现的普遍泄漏。

来源：[crates/agent-process/src/watchdog.rs:1–365](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-process/src/watchdog.rs#L1-L365)、[crates/tool-runtime/src/tools/process.rs:970–1240](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/process.rs#L970-L1240)。

## 架构判断

继续保持唯一执行权威与派生视图。此次主要问题不需要新框架：

1. 持久 owner 必须跨 await 保留；锁只解决互斥，不能代替事务/取消语义。
2. 队列单批有界不等于整个操作有界；输出缓冲有上限不等于排空循环有上限。
3. DTO 中存在字段不等于正式入口填写字段，也不等于 supplier 收到字段。
4. 日志中有事实不等于执行者 LLM 看到了事实；退出状态、正文覆盖和证据缺失都应在最终输入检查。
5. 当前有效的上下文才有资格争取复用。权限撤销、文件版本变化、用户纠正及硬预算不得为了缓存而延迟。

先修 owner/删除许可，再完成 tool truth 和 KV wire。避免再增长纯展示指标、总门禁数量、文档状态系统或 GUI 面板。

## 供应商 KV 验收范围

不按字节公共前缀直接宣称服务端 token 命中，不按输入变短直接宣称费用下降。第一阶段只做本地/模拟 HTTP 的正确接线；真实付费任务必须单独注明端点、模型、配置、调用间隔、缓存读/写/未命中、输出、维护与每次重试用量及未知数。

建议比较单位为“同一起点、同一目标、同一验收质量的已完成任务”。任何成本桶正规化都遵循具体端点的包含关系，未知字段不補零。先保护策略 B0 和真正稳定有效证据 B1；协议尾缓存只在完整 call/result、角色与端点边界均得到验证后另做一片。不要为了缓存新建一份无界历史快照。

## 外部规范（用于核对接口，不是实测结论）

- [Rust ExitStatus::code](https://doc.rust-lang.org/std/process/struct.ExitStatus.html#method.code)：Unix 信号终止不提供普通退出码，所以不能用 Option<i32> 代替进程是否退出。
- [MCP 2024-11-05 tools/list](https://modelcontextprotocol.io/specification/2024-11-05/server/tools)：该固定协议版本已定义 cursor/nextCursor。N10 不要求先升级协议版本。
- [OpenAI Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)：缓存参数与行为需按已确认端点配置。路由 key/前缀声明不是命中收据，不能推广为所有 OpenAI-compatible 网关的通用机制。

以上仅作小范围规范核对；本报告不建立统一供应商价格表，也不宣称任何实际节省百分比。
