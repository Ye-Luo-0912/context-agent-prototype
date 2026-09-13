# 下一阶段后端实施切片 · 续接现有队列

基线：`6eda247401092a18bcbd547e475bc79b1ed3a4c1`。本文件对应 `REVIEW_6eda2474.md` 的 N01–N10；编号只是报告定位。其他长期方向不变：主体后端优先，GUI 不扩展，供应商 KV 布局同步推进。不创建新的 scheduler、任务 authority、GC 框架、收费实验总门禁。

## 开工规则

先确认实际工作树 SHA，并用源码核对报告是否已过期。已修项目只验证接线，不重新实现。每次切片包含一个实际执行动作/故障与最小必要回归；定向通过后提交该片并推进下一主线。共享 contracts/prompt 边界由同一个集成人收敛，避免各线制造近似重复字段。

所有下面的回归均是**待添加和执行**，不是现有测试名或已通过结果。不要用一个不存在的过滤名得到0 tests，再把它记为通过。

## B 线：Context / GC / 持久恢复

### B1：统一删除许可（N01）

改动范围：context-simple store/reconcile + context-service startup + 必要服务回归。

先把 `roots_complete=false` 的卡片删除反例固定；然后覆盖同 store 服务重启。仅修“不完整保护”但启动仍传完整空根，不算完成；仅保留 ownerless blobs 但删除对应 card，也不算完成。

验收产物：保留 checkpoint 的卡片及原文跨服务退出/重启/restore 可读且元数据一致；真正孤儿在完整根条件下依然可清理。不要借机重写 checkpoint 格式或引入全新根管理器。

### B2：取消安全的冷页所有权（N02）

计划取样不移除 owner；只有验证与安装成功才完成迁移。临时错误/取消保留可重试 locator。单 id 和批次都覆盖；成功与失败混合批次不丢前后条目。

验收产物：gate 控制一次真实 card 读取，abort 前后 owner/diagnostics/manifest 保守一致，再读可恢复。上层可能做回滚不替代底层取消安全。

### B3：统一有界与校验读取（N03）

复用 blob reader 的资源保护；卡片验证 capture checksum 与结构；晚到页不绕开 restore 的结构约束。处理 Missing/Corrupt/IoFailed 不强行归一。

停止在同级资源/一致性保护，不建设新数据库。规模残余如 hydrate_all 总工作量沿原任务排队，并明确当前支持规模。

## A 线：执行核心与工具事实

### A1：session 状态与可控轮询（N07/N08）

退出状态与输出状态分离；model_content 有退出码/信号/成功信息。注册表锁不跨所有会话的慢 I/O；每次轮询有整体 work/time/cancel 预算。

验收产物：两会话并发，其中一个持续写/慢盘；另一个停止操作仍及时有结果。终止进程不再被无限显示 running，EOF 与 exit 顺序均正确。

### A2：真实退出后 grace（N09）

shell.exec 与 process.run 同类控制流一起修；统一从退出时刻计算排空预算。Full output 只在确实满足采集完整性时使用。

验收产物：退出后准时到达的尾部 sentinel 不丢；超出 grace 时明确 incomplete，不伪造完整日志。

### A3：MCP 完整发现（N10）

沿已有2024-11-05客户端支持分页。每页受限之外还有总预算，检测 cursor 循环、重复工具名与部分失败。不要新增协议版本迁移或插件管理 UI。

验收产物：第二页工具从正式 adapter 被发现和调用；失败/超额不静默形成完整 manifest。

## C 线：Runtime → supplier KV 的真实接线

### C1：让 key 与边界真的到达 wire（N04）

接口所有权：contracts 与 mapper 由 C 线维护；actor/model 生产填写点由 Runtime 所有者集成。最后验收人必须检查同一条 Compose → actor → transport → HTTP 链路。

key 是稳定隔离/路由命名空间，不是每轮内容 hash；同任务段按配置保持稳定，跨隔离域不可误共享。多边界的合法性基于最终 packed messages 与 tools；适配时考虑空消息过滤，不直接复用错误的 wire 数组下标。

验收产物：由真实生产组装路径发出的两次模拟 HTTP，非空正确 key，B0/B1 都存在，变化只影响应变化的区域。未知能力端点兼容路径保持不发专属参数。

### C2：空集与边界验证（N05/N06）

新输入明确 Some{0,0}；只有旧序列化输入走 None 兼容。取消正文 sentinel 猜结构。checked_add + 所属层长度验证先于任何切片。

验收产物：空 history + volatile evidence 的边界只含有效稳定层；非法 split 不 panic，消息/角色/调用配对不丢。

### C3：成本对照（条件任务，不阻塞前述功能）

先读回最终 wire 验证正确性，再按已有 live harness 做固定任务对照。禁止自动启用 ignored 付费测试。没有调用预算/凭据就记录 NOT_RUN，不新建占位“已节省”报告。

比较质量、总模型轮次、主/维护 lane、每次真实 attempt、缓存读/写/未缓存输入、输出、时延与未知用量。供应商桶正规化按端点语义；失败/取消尝试必须有已知下界和缺测状态。缓存命中不是 completion proof，也不授权复用副作用。

## 集成检查与停止条件

延续原分线名称：A 执行核心/工具，B Context/GC/搜索，C 平台/供应商 KV。本文先列 B，是因为恢复保全的优先级更高，不是更换架构职责。

A/B/C 可以并行，不要求修到“所有边界完美”才继续功能。但 N01/N02 的数据保全是发布前要求；其他切片各自相关测试通过即推进下一项。

有限的收尾旅程：

1. 一个跨模块真实任务，通过现有 headless/host 接收、转向、停止、恢复、验证、交付。
2. 同一任务触发外置/分片/服务重启，恢复 metadata+body 原事实，不重新制造第二套状态。
3. 长进程与分页 MCP 工具实际参与执行，最终模型能看到正确结果。
4. 至少两次由生产链路组装的请求验证稳定 key/B0/B1，并在条件允许时附真实供应商用量对照。

GUI 仅必要兼容修复。代码审查附加小问题不自动变成新的发布总门禁；高风险恢复缺陷例外。

## 建议执行命令（本轮未运行）

先沿仓库 CI 既有 fixture 构建和平台设置执行，以下是覆盖包选择，不替代其已有准备步骤。当前代码 CI 钉住的 Rust 工具链以仓库文件为准。

```bash
cargo build -p agent-process -p agent-context-service -p agent-host
cargo test -p context-simple -p context-contextcore -p agent-context-service
cargo test -p tool-runtime -p agent-capability-process
cargo test -p agent-contracts -p provider-openai -p agent-runtime -p agent-compose
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

跨线合并时按 CI 跑相关集成；阶段结束一次完整矩阵，不是每条小修改都重跑付费任务或增加一个测试框架。

## 每片回执最小字段

固定 SHA；改变的真实用户动作/故障；对应生产调用链；执行的非零测试清单及结果；未执行平台/供应商场景；兼容与资源边界；仍未完成的接线。不要只写“字段落地”“fixture绿”或“文档工单关闭”。
