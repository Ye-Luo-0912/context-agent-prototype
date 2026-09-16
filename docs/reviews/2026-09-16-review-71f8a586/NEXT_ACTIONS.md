# 当前实施任务：71f8a586 续审

基线 `71f8a58614fcabbbc9bc9602fe85ef453df56765`。先核对工作树与后续提交；以下是修改建议，没有在用户仓库应用。共享契约由一个集成人维护，行为修改与机械搬文件分开提交。GUI 仍后置。

## 0. C0：调查 Windows 实际 CI 阻塞（A）

目标：宿主被终止后，被监督的验证进程树完整退出，不能只 leader 退出。

输入：CI 35156892711 / Windows job 104999146984；见 CI_OBSERVATION.md。先读 proof_supervision.rs 和真实启动/Job 关联路径，保留准确 identity 检查。

检查命令（Windows，尚未由本审查环境执行）：
```powershell
cargo test -p agent-compose --test proof_supervision -- --nocapture --test-threads 1
```

增加必要确定性屏障/回执后复验正常、已降级、宿主被杀和成员退出路径。不能依靠 worker 自超时使测试通过，不直接 ignore，不因重跑一次成功就宣称根因已消除。停止条件：故障机制被解释并回归，或明确保留未解决阻塞与证据；不牵连 unrelated 功能重写。

## 1. A/B：可继续且不漏内容的取回（F1/F2/F6）

用户动作：按工具给出的参数一路读取工件，包含超长单行尾部；符号搜索不完整时能准确知道未覆盖范围。

修改入口：artifact.rs 参数/游标/页生成；code.rs coverage；必要时复用既有 artifact locator、正文预算契约。先写三个红例：默认续读201；3MiB单行后半sentinel；code.symbols 空命中但扫描未完。

实施：统一解析与下一批参数生成；缺省 end_line 相对 start_line；行内继续需稳定位置，不仅增加 has_more；可见正文明确不完整。最终输出经 broker 再验证。

定向候选命令（测试名以实际新增为准）：
```bash
cargo test -p tool-runtime artifact
cargo test -p tool-runtime code
cargo test -p agent-conformance
```

停止：所有返回的继续参数可执行、位置单调推进、EOF 有真实依据，原始内容不靠重复首行才能读取。小型完整输出保持原语义。不要引入向量检索、AST 服务或新的存储引擎。

## 2. A/C：参数契约与数值规范化（F3/F4）

用户动作：模型受到它实际看到的工具 schema 约束；其他语言对同一合法参数生成一致 canonical bytes。

修改入口：SchemaProfile 编译/校验，JCS 小数格式，既有 Core schema-mismatch 路径。E2 非精确整数拒绝规则保留。

回归：enum[false] 不接受 true；无 type 的嵌套约束不静默丢失；unsupported 在 admission 拒绝；schema mismatch 不到达审批和执行。浮点 ±1e-7、±1.2e-7、1e-6、1e-8 和大指数阈值，RFC Appendix B 与 Node/reference 字节对照。

```bash
cargo test -p agent-contracts schema_profile
cargo test -p agent-contracts jcs
cargo test -p agent-core
```

兼容：给持久摘要变化明确迁移/版本策略，不能放宽校验或直接重算旧 WAL。停止：已接受约束有执行语义，数值规范化符合选定契约，老记录处理明确；不扩张到完整 JSON Schema 实现。

## 3. A：通用进程发送取消（F5）

用户动作：子进程不读请求/答复时，取消仍及时停止，不等30s请求截止。

修改入口：agent-process::ProcessHost 的首次请求写和 system reply 写；抽出可复用的有界发送机制。MCP 已有不同实现的通过记录，不要重复替换其正常语义。

回归：handshake 后停读、真实部分写阻塞、取消、poison/reap；broker reply同样覆盖；写前取消可复用；半帧后不能复用或自动重发 mutation。等待发送权也纳入预算与归属判定。

```bash
cargo test -p agent-process --test host
cargo test -p agent-capability-process --test capability_process
```

停止：读、写、broker处理各阶段都有准确取消/期限结果；无无限等待，不遗留可被后续请求错用的半帧。

## 4. C：生产装配序列 KV 验收

保留 provider 手工八步矩阵、既有 compose two_production_requests smoke。新验收放 compose 测试层，以真实 Runtime/工具/Context 驱动变化，不在 provider 反向依赖 runtime。

同 TaskId/workspace 从读取到 checkpoint/恢复，覆盖焦点、缺失、新证据、文件版本、工具撤销、F1/F2工件取回、失败/取消账目。捕获实际发送的最终 HTTP，比较稳定前缀与工具 schema、首差异原因、有效正文、已知/未知usage。

```bash
cargo test -p provider-openai task_sequence
cargo test -p agent-compose --test cache_wire_flow
cargo test -p agent-compose --test cache_routing_wire_acceptance
```

真实供应商调用需要明确预算/凭据/授权；本任务不自动运行 paid/ignored 测试。记录 LOCAL_WIRE、ENDPOINT_ACCEPTED、SERVER_HIT、NET_TASK_COST 各自状态。未运行保持 NOT_RUN。停止：本地真实装配序列满足语义且变化可解释；不以手工固定帧的稳定代替实际稳定，不报告未测百分比。

## 合并与文档

每片先定向，再跑受影响 crate 及既有跨 crate 回归；合并后沿现有 CI 收尾。不要每个小改动重跑全部付费长任务，也不要增加平行权威状态。CURRENT/NEXT_TASKS 仅替换当前动作、限制和原回执链接。
