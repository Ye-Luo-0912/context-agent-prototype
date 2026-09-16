# 71f8a586：后端长流程、取回契约与验证边界续审

## 基线与证据等级

- 仓库：Ye-Luo-0912/context-agent-prototype。
- 固定 SHA：`71f8a58614fcabbbc9bc9602fe85ef453df56765`。收尾分支查询仍为该 SHA。
- 提交时间：2026-09-16 22:16:57 UTC / 2026-09-17 07:16:57 Asia/Tokyo。
- 本轮实际读取 25 个不同源码、配置、文档文件的全文或区间，另读目录、变更和 CI。**不等于全仓逐文件逐行覆盖完成。**覆盖细节见 COVERAGE.md。
- 源码经 GitHub 连接器读取；本地 GitHub Git 获取遇到 DNS 失败，环境无 Cargo、Rustc、.NET。没有执行仓库 Rust/.NET/真实终端回归，没有调用付费供应商，没有修改或推送仓库。
- `mechanism_probes.py` 已在本环境执行。它是局部算法移植与实际 Node.js 参考结果对照，不是 Rust 测试，不模拟完整 Runtime。
- 下文 F1–F6 为源码确认的缺口或条件反例；C0 是实际远端 CI 失败，根因尚未确认。未证明权限绕过、远程利用或普遍数据损失，不提升为此类结论。

## 结论与优先级

保持原后端三线和 GUI 后置：A 执行核心/工具/TUI 操作入口；B Context/GC/搜索；C 平台/供应商 KV/成本。

先调查 Windows 验证进程树的实际 CI 失败。并行完成三组代码工作：取回链路 F1/F2/F6、参数契约 F3/F4、通用进程写入取消 F5。KV 保留现有 mapper 和手工序列测试，在已有 compose 测试处补生产装配序列，不新建框架。

## C0：Windows CI 的验证进程树清理失败

CI run `35156892711`，attempt 1，最终 failure。六个 job 成功，Windows full Rust test job `104999146984` 失败。Linux 两个分片此次均成功，新增 agent-host 运行覆盖已生效。

失败测试：`agent-compose --test proof_supervision` 的
`killing_the_rust_host_cleans_the_exact_proof_tree_without_a_completion_receipt`。
日志在 `proof_supervision.rs:171:9` 报：leader 已 Exited，成员仍以同一个 identity_token 处于 Running，20 秒窗口结束仍未确认完整退出。摘录见 CI_OBSERVATION.md。

这证明该次实际环境没有通过声明的清理保证；不能据此直接宣称所有生产启动都会漏进程，也不能未调查就归为负载抖动。该测试使用真实 crash_child，故意杀宿主；Windows 为 TerminateProcess，Rust Drop 不会执行。测试显式区分 PID 重用，并把等待时间置于 worker 自退出时间之前。

**下一动作：**在 Windows 对照 job assignment/继承、宿主死亡、leader/member 身份与退出顺序，确定是实现竞态还是 fixture 前置条件不充分。必要增加有界且可归因的 arm/assignment 回执。不要延长到 worker 自退出后让测试假通过，不移除成员检查，不以 Linux 成功覆盖 Windows 结论。

## F1 · P2：artifact.read 发出的续读建议不满足自己的默认参数

入口：`crates/tool-runtime/src/tools/artifact.rs`，`ArtifactReadArgs`、`default_end_line`、`ArtifactReadTool::execute` 与 coverage footer。

当前 start_line 默认 1，end_line 独立默认 200。footer 只建议 `reference + start_line=next`，未指定 end_line。对 450 行短文本，首批 1–200 后建议 start_line=201；照提示调用时实际范围成为 201–200，立即被拒绝为 invalid line range。

这是普通合法工件和默认参数就能到达的问题，直接增加模型修参/重复读取轮次。它不是权限问题。

**修复：**end_line 缺省时按 start_line 派生有界范围，使用 checked arithmetic；或让生产者返回完整且可直接执行的下一批参数。生成与解析共享类型化语义，兼容旧显式 end_line。不要靠提示词要求模型记住额外规则。

**回归：**真实工具→broker→模型可见返回中提取 continuation，按原样继续，直到 500 行以上 fixture 的 sentinel 全部出现。检查重复、遗漏、终止、极值与范围拒绝。测试不得手工额外补齐与产品返回不同的参数。

**本地证据：**机制移植结果 `rejected_as_invalid_range=true`；Rust 路径未执行。

## F2 · P2：超长单行的尾部没有续读地址，并可能误报工件结束

同一文件。单次 capture 上限约 2 MiB，扫描上限 8 MiB。当一行大于剩余 capture room，代码只保存前缀并补换行，却把 `last_captured_line` 设为整行已处理。后续游标以 `last_captured_line + 1` 计算；`has_more` 不包含 `captured_truncated`。

一个 3 MiB 单行工件（仍在扫描预算内）即构成：正文被截断、scan_complete=true、counted_lines=1、has_more=false，最终输出“end of artifact”。如果后面还有行，继续点直接越过首行未展示的尾部。

原始工件并未被物理删除；问题限定为 LLM-facing artifact.read 缺少行内/字节级续读。平台另有读取接口也不自动使这个工具的承诺成立。即使再次请求首行，也只是重复相同前缀。

**修复：**为超长单行提供绑定不可变工件身份的字节/块/行内游标；只有实际消费到相应位置才推进。兼顾 UTF-8 边界，明确转换显示和原始位置关系。元数据 partial 与模型正文提示一致。还要经过最终 16k 输出经纪，避免在工具内部 2 MiB 通过后又被下一层截成无法继续的预览。

**不做：**增大上限代替继续能力；无界读全文件；仅把 has_more 置 true 却仍返回同一位置；把新预览工件当作原工件尾部。

**回归：**单行、长首行加短次行、CRLF、UTF-8 多字节、字节上限附近。sentinel 放在被截行的后半部分，而不只放在下一行。每次游标前进，结束必须确实没有未展示区间。

**本地证据：**机制移植中 `window_truncated=true`、`suffix_sentinel_visible=false`、`reports_end_of_artifact=true`。

## F3 · P2：Schema 编译接受约束，却在节点构造中静默丢弃

入口：`agent-contracts/src/schema_profile.rs` 的 `BoundedNode::compile/validate`；真实消费者在 `agent-core/src/kernel/mod.rs::execute_published_tool`，先校验再审批。

例子：根为 object，其 flag 属性是 `{"type":"boolean","enum":[false]}`。编译器读取并检查 enum_options，最后却构造不携带它的 `BoundedNode::Bool`；验证仅检查布尔类型。true 因而不被这项枚举约束拒绝。

另一个同类分支是缺少 type 的嵌套 schema：pattern/properties/required 被读取，却在 `NodeType::Any` 中不保留，造成接受即弱化。这里不是要求支持完整 JSON Schema；如果子集不支持该组合，应在 admission 明确拒绝，不能成功编译后忽略约束。

上轮 E2 的非精确整数与 Any 深度/数值域检查已有修复，不重开。Schema 是形状门禁，不是副作用授权；本轮没有证明可绕过 Core 权限。

**修复：**保留独立约束（布尔 allowed set/通用 enum 层等）或拒绝不支持组合。编译后的 profile、渲染给模型的 schema 和 dispatcher gate 必须同义。对空 enum、注解与约束、无 type 节点明确分类。

**回归：**compile→validate→Core no-dispatch；flag=true 对 enum[false] 拒绝且不进入 approval/dispatcher，false 正常。无 type 的嵌套 string/object 约束要么执行正确，要么编译拒绝，不静默丢弃。示例见 SCHEMA_CASES.json（测试规格，不是已执行的 Rust 输出）。

## F4 · P2：JCS 小数指数边界错误，跨实现摘要不同

入口：`agent-contracts/src/jcs.rs::scientific_from_ryu`。Cargo.lock 的 Ryu 为 1.0.23。

`point = exponent + 1`；当前 `(-6..0).contains(&point)` 接受 point=-6，所以 Ryu 的 `1e-7` 被改写为 `0.0000001`。JCS 采用 ECMAScript 数值格式，这一数应输出 `1e-7`。同类输入还有 `1.2e-7` 和 `-1e-7`。

本轮实际 Node.js v22.16.0 对照确认了上述规范输出；局部 helper 移植的 8 个样本中 3 个字节不同。`1e-6`、`1e-8` 和若干大指数对照保持一致。它不改变数值，不是 SHA 碰撞；问题是本地 canonical bytes 不符合跨语言契约。ArgumentDigest 已用于 Core 参数身份校验，因此要维护旧持久摘要兼容，不能直接重算历史记录后宣布失效。

**修复：**按规范修正 plain/scientific 的开闭边界，补完整 Appendix B、指数阈值两侧和正负值，再做有限浮点 bit-pattern 差分。若替换数值格式库，先验证序列化行为与历史兼容，不能仅因为都使用 Ryu 就假定输出格式一致。

**兼容：**保留旧编码版本解释或明确迁移边界；不得通过放宽摘要校验来兼容，不对历史 WAL 批量重写。

## F5 · P2：通用 ProcessHost 在发送阶段没有响应取消

入口：`agent-process/src/host.rs::exchange_once`，以及 `session.rs` 的发送实现。生产调用链：`ProcessCapabilityAdapter::invoke`→`call_with_cancel_and_broker`。

当前首次 `send_encoded_line(request)` 和 broker 答复的 `send_encoded_line(answer)` 都是直接 await。取消检查主要位于首次发送之前、读响应 select 和 broker.handle select。子进程不再读取时，管道背压可以卡在发送阶段；此时 token 已取消也只能等外层 request_timeout（生产配置 30s）等返回。

外层有超时，所以不是永久无界等待。但它未兑现与读阶段相同的及时取消保证。MCP 客户端另有 cancel-during-write 回归，本次 Windows 日志中也通过；不能把 MCP 的通过当作通用 JSON-lines host 的覆盖，也不能把本发现写成 MCP 已坏。

**修复：**将写/flush 与等待发送权的取消语义集中在一处，采用剩余期限与 cancel。发送可能部分发生时，必须 poison/discard/kill/reap，并保持结果 Unknown/Cancelled 的准确边界。不能取消 write future 后继续复用这条半帧连接，也不能自动重发副作用请求。

**回归：**真实可控子进程完成 handshake 后停止读 stdin；请求大于管道缓冲且在合同上限内，确认部分写入已开始后取消，要求在取消预算内结束而非等待30s。broker 大答复也覆盖。另测写前取消保持连接、部分写后连接失效、没有错误归属后续响应。Rust 测试未在本环境执行。

## F6 · P2：code.symbols 已知扫描不完整，却没有在模型正文中报告

入口：`tool-runtime/src/tools/code.rs::CodeSymbolsTool::execute`。

`walk_incomplete || symbols.len() >= limit` 只写 metadata。model_content 在无命中时是 `no symbols found`；有命中只显示行和“已收集结果”的工件分页信息，没有把未扫描文件/命中上限导致的覆盖缺口带进去。

词法扫描而非 AST 是已声明的设计取舍，不把它本身列为 bug。缺口是系统已知的部分覆盖没有到达执行者 LLM。“结果工件还有下一页”也不等于“扫描能继续到剩余文件”。

**修复：**沿既有 coverage footer 显示 partial、stop reason、已扫描范围与结果 cap，区分未搜索候选与仅显示前 K 个。只有真正实现的扫描续跑才能发 continuation；否则提供缩小目录/条件等诚实下一步。

**回归：**非空受限结果、空命中但目录扫描未完，以及小型完整对照，经过 producer→broker→最终 tool message 检查正文。不得只断言 metadata=true。

## KV 与维护性：补生产序列，不推翻现有测试

新 `provider-openai/src/task_sequence_tests.rs` 是从手工 ModelInput 开始的序列，真实调用 into_request/mapper，因而可以验证给定帧的传输行为。它没有驱动 Runtime 的选材、必需正文解析、最终 packing、GC 和 checkpoint 后的再组装。回执也说明了这个取舍。

同时仓库已有 compose 层两次真实生产请求共享 key/B0/B1 的 smoke，Windows 日志中通过；**不能宣称完全没有 Runtime cache 测试。**尚欠的是把已有两请求 smoke 与多变化矩阵连成同一条生产轨迹。补读 cache_wire_flow.rs 还确认，该 fixture 使用 read_only 审批，未断言 evidence.txt 成功写入；非空选材与 B0/B1 出现本身不能证明写动作已成功。扩展旅程时应验证实际工具结果和磁盘产物，而不是只沿用“发过 fs.write”的注释。

在 `agent-compose/tests` 延长同一任务：真实读取/长工件续读→新证据→焦点改变→文件版本改变→工具撤销→checkpoint/恢复→取消/失败结算，捕获最终 HTTP。共享依赖层允许这样接线，不把 provider 反向依赖 runtime。

每次观察 key、tools、有效 prefix、首差异位置/原因、正文覆盖、usage 完整性。真实端点接受、服务端命中、同质量净费用三项单独记录，未执行保持 NOT_RUN。无需新评测框架，不以“省掉实际需要的正文”换缓存收益。

`execution/memo.rs` 仍明确未接入 dispatch、lookup 恒 miss，这是预留骨架，不应误报为生产语义读缓存已上线。暂不为填满模块而接入不安全的工具复用。

## 文档与阶段退出

保留 E1 的覆盖失效规则、E2 的数值域准入、E3 的 Linux host 分片和 E4 的 CString fixture 修复；不照旧重开。当前队列只写 C0 与四个实施切片，关闭过程链接到原回执。本报告不要整体追加回 CURRENT/NEXT_TASKS。

阶段退出仍以真实任务链为准：合法续读可以执行且不漏行内尾部，已接受的参数合同不会被编译弱化，取消在发送阶段也有界，最终请求与缓存账目可核对。GUI 后置；不要求所有诊断小项清零后才交付主体。

## 主要源码与外部依据

- [crates/tool-runtime/src/tools/artifact.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/tool-runtime/src/tools/artifact.rs)
- [crates/tool-runtime/src/tools/code.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/tool-runtime/src/tools/code.rs)
- [crates/agent-contracts/src/schema_profile.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-contracts/src/schema_profile.rs)
- [crates/agent-contracts/src/jcs.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-contracts/src/jcs.rs)
- [crates/agent-core/src/kernel/mod.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-core/src/kernel/mod.rs)
- [crates/agent-process/src/host.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-process/src/host.rs)
- [crates/agent-process/src/session.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-process/src/session.rs)
- [crates/agent-capability-process/src/capability_host.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-capability-process/src/capability_host.rs)
- [crates/provider-openai/src/task_sequence_tests.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/provider-openai/src/task_sequence_tests.rs)
- [crates/agent-workspace/src/broker.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-workspace/src/broker.rs)
- [crates/agent-runtime/src/output.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-runtime/src/output.rs)
- [crates/agent-compose/tests/proof_supervision.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-compose/tests/proof_supervision.rs)
- [.github/workflows/ci.yml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/.github/workflows/ci.yml)

外部一手规则（读取于本轮）：
- RFC 8785 §3.2.2.3、Appendix A/B：https://www.rfc-editor.org/rfc/rfc8785.html
- JSON Schema enum：https://json-schema.org/understanding-json-schema/reference/enum
- Ryu 1.0.23 源码文档：https://docs.rs/ryu/latest/src/ryu/pretty/mod.rs.html
- OpenAI Prompt Caching：https://developers.openai.com/api/docs/guides/prompt-caching/

具体源码是本仓库固定 SHA；外部规则只支持格式/契约说明，不代替代码证据。历史工作树、未提交改动与供应商实测不在本报告范围。
