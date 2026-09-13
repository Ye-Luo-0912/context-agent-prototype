# 长流程后端续审：供应商 KV 缓存与请求布局

日期：2026-09-13  
审查基线：`489c89cd7d139f4e721348ec9c54054700d93b7b`  
仓库：Ye-Luo-0912/context-agent-prototype  
性质：代码静态续审、供应商官方文档核对、实施任务；不是已实施补丁或性能验收回执。

## 1. 保持不变的范围

继续优先完成执行核心、工具、Context/GC/搜索和最小平台组成的长流程主体。GUI 只维护必要兼容与安全修复。供应商 KV/prompt caching 是本轮 C 线的正式交付对象，不再只用本地正文缓存或工具 memo 代表“缓存优化”。

优化目标是通过验收的任务总成本和可靠性，不是最高命中率、最短输入或最多测试数量。缓存不能延长失效授权、让旧文件冒充当前版本、删除必需指令或把普通工具输出提升为 system 权限。

### 验证边界

本轮实际读取了 GitHub 上固定 SHA 的请求契约、复用边界、PromptAssembler、工具表面计划、最终 packing 的相关路径、provider 映射/诊断/重试、compactor、checkpoint 校验和 search.grep 新实现，并核对最新 CI 作业与失败日志。此前计划 `LONGFLOW_BACKEND_PLAN.md` 已读取，用于避免重复派发已修事项。

未完成全仓所有文件的逐行覆盖。容器尝试访问仓库时 GitHub DNS 解析失败，且未找到 Cargo/.NET；未克隆成功，未执行本地 Rust/.NET 测试，未调用真实付费模型，未修改或推送用户仓库。供应商规则只说明已确认端点应具备的能力；不能据官方上游文档推断第三方网关一定转发参数或传递折扣。

### 当前集成信号

该 SHA 的 CI run `34754942152`、attempt 1：读取时 Linux/Windows 的 fmt/clippy/build、.NET、文档检查和 Linux part 1 成功，Linux part 2 失败，Windows 全量测试仍运行中。

Linux part 2 日志明确失败于：

```text
crates/agent-compose/tests/proof_supervision.rs:102:9
killing_the_rust_host_cleans_the_exact_proof_tree_without_a_completion_receipt
 timed out: exact proof tree exit
```

日志在超时点把 leader/member 的确切进程身份报告为 Running。它证明这次 CI 没有确认期望的进程树清理，不足以单独定位是生产监督逻辑、夹具还是环境根因；不能默认按抖动忽略。运行记录见 [CI 作业](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34754942152/job/103718012229)。

## 2. 旧问题核对：不要重新建设已完成的部分

| 旧项 | 本轮观察 | 队列处理 |
|---|---|---|
| F1 spill 校验 | `spilled_entries_from_value` 已严格解析；`reject_duplicate_spill_ownership` 覆盖四类内联 owner 及清单自身重复 | 原缺陷不按原描述重开；不等于本轮完成全部恢复路径测试 |
| F3 正文恢复需求按文件身份合并 | 已改成 `Vec<FileBodyWindow>`，需求减去真实窗口覆盖，cache/spill 都验证需求范围 | 已在源码核对，退出旧待办 |
| F4 扫描不能继续 | 新增 sealed `scan_continuation`，与结果分页 cursor 分离 | 后端已有实现，但还有模型 schema 接线缺口，见 R7 |
| F5 平台长任务控制 | 当前合并提交及 CURRENT 报告已接入 steer/activate/suspend、正式 checkpoint、精确 continue/cancel 和预算入口 | 不再重新派发整项；这里没有重跑它的端到端验证 |
| F2 冷元数据 | CURRENT 报告已把 checkpoint 卡片 I/O 移出 state 锁，restore 初始读取有批次预算，后续按需加载 | 保留文档明确的规模边界；不宣称所有已加载元数据都已可驱逐或支持无限历史 |

直接源码证据：[R-A](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/context-simple/src/checkpoint.rs)、[R-B](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/agent-runtime/src/prompt.rs)、[R-C](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/tool-runtime/src/tools/search.rs)。F2/F5 的实现/验证状态区分为仓库回执来源：[CURRENT](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/docs/CURRENT.md)。

## 3. 本轮发现

“优先级”表示实施顺序，不表示已证明安全漏洞或已测量费用损失。以下成本后果由实际代码路径与供应商语义推导，尚未用生产账单量化。

### R1：唯一复用边界包括了易变的整个 context_frame

**优先级：高；类型：布局/成本设计缺口。**

位置：`agent-contracts/src/model.rs::ModelInput::into_request`、`agent-runtime/src/prompt.rs::assemble_with_catalog_stats`。

CurrentStateLast 的边界长度为 system_policy 的消息数加整个 context_frame 的消息数。context_frame 内顺序为：Foreground → Selected working context → Required misses → External refs → Restored turn bodies。之后才是 turn_frame、current_state_frame 和 focus_frame。

这意味着“名称叫 context”被当成了一个整体复用区域，但其中会因新检索、缺失状态、residency、checkpoint 恢复改变。最前面的 Foreground 改动还会截断后面的相同历史正文。对于要求已写入的精确断点前缀的端点，一个末端断点不能替代较早的稳定断点。

**反例：** 两次请求共享 S+E，仅缺失提示 M1→M2；当前唯一断点在 M 后。即使 S+E 相同，也没有由当前实现主动建立的 S+E 独立断点。第三方是否自动保留别的前缀不能由本地推断。

**实施：** 把稳定证据段与易变投影分开，先支持少量有界的合法前缀边界，至少容许“稳定基座”和“本阶段证据”两层。新检索内容应立即可用，但不必立即重写早期稳定证据段。

**停止条件：** 动态缺失/foreground/恢复正文变化不改稳定段的实际字节和有效边界；真正的版本/权限变化立即使相关段失效。不得只让 metadata 中的摘要不变而正文已变。

源码：[ModelInput](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/agent-contracts/src/model.rs)、[Assembler](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/agent-runtime/src/prompt.rs)。

### R2：显式 Responses 断点已发送，但没有 prompt_cache_key

**优先级：高；类型：已支持端点的接入缺口。**

位置：`provider-openai/src/lib.rs::build_responses_wire_request`；Chat mapper 也没有发送该字段。

当前已有 `prompt_cache_breakpoint` 和 `prompt_cache_options.mode=explicit`，不能说仓库没有 provider cache 支持。缺的是与端点能力绑定的稳定路由键。`PromptReuseBoundary.prefix_digest` 是对前缀和工具的完整性绑定，不是已发送的供应商 cache key。

**实施：** 在确认支持的 profile 中提供稳定、不含明文敏感信息的路由命名空间。可按安全隔离域/工作区、端点配置、任务和 Main/Maintenance lane 确定。同任务连续请求保持稳定；不要使用每轮 UUID、model_round、完整请求 hash 或易变上下文集合 hash。冷恢复是否复用同一键按持久任务身份决定，不能因为 run 进程重启就无条件换键。

键不是授权凭据，也不证明缓存内容正确；内容完整性仍按实际前缀和工具检查。未知/不兼容端点保持不发送专属字段。不能根据域名或模型别名猜测原生能力。

**停止条件：** 实际 HTTP payload 中可见正确键；同隔离域/同任务连续请求稳定，跨隔离域不混用；默认兼容端点的 payload 不改变。

源码：[wire builders](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/provider-openai/src/lib.rs)、[integrity boundary](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/agent-contracts/src/model_cache.rs)。

### R3：无有效边界时，显式模式退回默认模式；compactor 走这条分支

**优先级：高；类型：费用策略不完整。**

位置：同一 Responses mapper，以及 `agent-compose/src/compactor.rs::compact`。

`prompt_cache_options` 只在 `boundary_index.is_some()` 时设置。缺失/过期 hint 走 ProviderDefault 同形请求；现有测试明确固定此行为。Compactor 直接构造 ModelRequest，只设置 role/folded_items/output_char_cap，没有复用边界。因此，即使选用显式缓存 transport，压缩请求也不会得到显式-only 写策略。

在已确认支持“explicit 且零断点不缓存”的端点，不能让“没有可缓存段”悄悄变成“让供应商缓存一次性后缀”。这是策略选择，不是要求所有请求必须有断点，也不是让错误 hint 阻塞任务。

**实施：** 分开“端点支持什么”“调用期望的写策略”“本次有无合法边界”。显式-only 已确认时，无可复用边界可以走无缓存写的已支持请求形状并记录原因。ProviderDefault/未知能力继续原样。主模型和维护调用分别选择预算与缓存策略。不要为低于缓存门槛的短 system prompt 填充无用内容。

**停止条件：** 主调用、压缩调用、空边界、过期边界、旧协议都能用最终 wire 测试说明有效策略；不自动把客户端 hint 拒绝升级成供应商能力猜测。

源码：[mapper](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/provider-openai/src/lib.rs)、[tests](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/provider-openai/src/prompt_cache/tests.rs)、[compactor](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/agent-compose/src/compactor.rs)。

### R4：相同正文因状态标题或跨层去重而改变位置

**优先级：高；类型：Context 与缓存的联动问题。**

位置：`prompt.rs::render_selected_item`、`omit_selected_file_body`、`visible_body_windows_from_parts`。

正文前带 `workspace_identity=current`、attention、semantic。正文还可能在 Selected、协议尾、Restored block 之间迁移；当前范围校验正确并不意味着迁移在 KV 成本上免费。每轮重新排列、移除前部正文或更改前部状态文字，都可能改变较长的后续前缀。

**实施：** 对仍合法且仍有价值的证据，在一个有界阶段内稳定身份、版本、范围和展示顺序。可变 currentness/注意力状态放到有身份引用的动态状态段。已失效证据不允许借“稳定”继续作为当前事实发送。去重与保留正文位置共同决策，不是全面关闭去重或把历史全部附加。

**停止条件：** 窗口迁移可解释；同一版本/范围的合法稳定证据不因纯诊断变化重排；冲突、撤销、用户删除和硬预算能立即打破缓存稳定性。

### R5：协议尾未纳入当前边界；扩展复用必须是单独的后续切片

**优先级：中；类型：潜在优化，不是现有协议错误。**

当前复用边界只允许 System/User 文本前缀；不覆盖 assistant/tool 协议。turn checkpoint 的动态说明被放在 turn 消息前部，保留窗口滑动也会改变这一段。不能声称只把 Focus 后置就已经复用了完整长对话。

第一片只保护合法稳定证据基座。第二片再在有界阶段内减少协议历史前部改写，保持完整 call/result 组和真实角色。供应商只允许特定内容块打断点时，必须按支持位置映射；不得为了打点，把 function_call_output 改成 user 普通文本，或随意解除边界验证。

### R6：重试后已知用量仍可能丢失

**优先级：高；类型：成本账目正确性。**

位置：`provider-openai/src/retry.rs::complete_stream_live/complete_stream_buffered`；`lib.rs` 的流内错误和 Done sink 错误出口。

retry 暂存最近一笔失败 usage，不是已知尝试总量；成功分支只返回成功 output，并盖 attempts/retries。等待重试时取消、部分 sink 错误直接返回，可能丢掉已获得的用量。供应商已写缓存的失败请求同样可能有成本，不能只统计最后成功一次。

**实施：** 在现有调用标识和事件道上记录一次 attempt 的最终用量/完整性，或形成可解释的有界聚合。每次尝试内部多个 SSE 累计快照只能结算一次；不同真实尝试才能相加。没有报告的尝试计 unknown，不补成零。取消保留已经收到的报告，但不为未收到的费用编造数值。

**停止条件：** failure-with-usage→success、两次 failure-with-usage→give-up、usage 后取消、sink failure 等情形，账目恰一次且已知数值不减少。

源码：[retry](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/provider-openai/src/retry.rs)、[stream exits](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/provider-openai/src/lib.rs)。

### R7：新搜索续跑参数未进入模型 schema

**优先级：先修；类型：工具表面与实现不一致。**

位置：`tool-runtime/src/tools/search.rs::{GrepArgs, SearchGrepTool::spec, execute}`。

GrepArgs/执行分支已有 scan_continuation；description 和 coverage footer 指导继续，但 input_schema.properties 仍只有 pattern/path/limit。解析器支持并不等于模型能稳定发现和生成参数；严格参数生成/校验还可能放大这一缺口。没有在本轮实测特定 provider 的拒绝行为，因此不表述为所有模型必然拒绝。

**实施：** 在真实 model-visible schema 声明该可选 handle、格式/长度约束和同查询语义；验证 compact_for_model_surface 和 provider wire 后仍存在。保留旧 cursor 仅作结果分页兼容，不与扫描 handle 混用。

**停止条件：** 用正式 ToolSpec 构建一次模型请求，按其 schema 生成的续跑参数可以走真实 dispatcher 到第二批，而不只是直接调用 execute。

源码：[search.grep](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/tool-runtime/src/tools/search.rs)。

## 4. 应保留的现有实现

- PromptReuseBoundary 对实际消息前缀和 tools 重新校验；过期 hint 不得被信任。继续保留这一完整性约束。
- RoundSurfacePlan 已把最终工具按名称排序。问题不是工具随机排序，而是加载/预算/任务需求导致的集合变化。不要把已做的排序另派工单。
- 文本输出的权限层级必须保留；缓存不拥有授权、任务完成和证据正确性真相。
- diagnostics.rs 已有最终 HTTP body、各 input item、tools、settings 的摘要和 cache read/write/miss 字段，并明确说字节前缀不是服务端 token 命中。扩展和接入现有观察器，不新建平行遥测框架。

源码：[surface](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/agent-runtime/src/surface.rs)、[diagnostics](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/489c89cd7d139f4e721348ec9c54054700d93b7b/crates/provider-openai/src/diagnostics.rs)。

## 5. 建议布局：先稳定基座，再做有限阶段

以下是建议，不是现有已经落地的实现。

### 第一片：证据基座复用

```text
稳定 policy / 稳定 runtime facts              -> 可用时设较早边界 B0
仍有效的阶段证据（固定身份、版本、范围、顺序）  -> 证据边界 B1
本轮新证据、检索/缺失/恢复投影                 -> 暂不承诺复用
完整保留的协议窗口                            -> 暂不扩大缓存契约
最新任务状态 / 当前指令与焦点                  -> 动态尾
```

此片不要求把整个对话放进缓存。边界只能出现在已支持的合法内容位置，至少先在现有 System/User 文本范围内工作。较早段不够最低缓存规模时允许跳过，不填充；合并/移动不改变来源与权限。

### 第二片：有界阶段（epoch）

让一个小阶段的证据快照和必要协议组保持前部稳定，追加本阶段真实新结果；到任务阶段切换、必要证据大幅更新或资源上限时重建。它不是新调度器、新任务数据库或无限 append-only，而是现有 final-packed 请求的有界派生视图。

现有 TaskAnchor、Context owner、工具授权仍是唯一真相。派生视图只保留满足来源有效性和字节预算的快照；它持有的 Arc/字节也必须计入真实内存预算。不能说底层已 GC，而另一个 prompt snapshot 无界保留所有正文。

每次出站的决策顺序应是：

```text
校验指令、授权、版本与必需证据
  -> 处理硬失效（必要时立即重建；不受缓存命中率阻挡）
  -> 判断当前基座是否仍有价值且在预算内
  -> 以最少必要变动装入新证据和协议组
  -> final pack 后生成/校验少量边界
  -> provider 按已确认 profile 映射实际 wire
  -> 记录用量和边界变化原因
```

普通 residency/attention 调整不应自动变成 prompt 全量改写。反过来，物理 GC 也不能被服务器缓存寿命完全控制。区分“结束当前请求中的引用”“本地热内存释放”“持久化保留根”“供应商缓存失效/过期”四件事。

### 工具表面

复用现有 ToolSurfaceSnapshot。仅减少没有业务必要的集合抖动；不得延迟 revoke、伪造可用工具或用一个全能 shell 替换正常 schema。真实工具变化可导致前缀重建，这是正确性成本，不应该被计为无意义失效。

## 6. 供应商差异与核算

核对日期：2026-09-13。实现以实际 endpoint/profile 为单位，不能仅依据 OpenAI-compatible 请求形状。

- OpenAI 当前官方说明：GPT-5.6 及以后按缓存断点的完整前缀匹配，写入可单独计费；早期模型行为不能直接推广到新端点。显式-only 写策略与稳定路由键应分别处理。见 [OpenAI guide](https://developers.openai.com/api/docs/guides/prompt-caching) 和 [Responses API reference](https://developers.openai.com/api/reference/cli/resources/responses/methods/create)。
- Claude 当前文档：缓存以累积前缀为单位，工具在前；写入 TTL 和读取的价格分开，input_tokens 口径不同于“全部输入总量”。见 [Claude guide](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)。
- DeepSeek 当前文档：命中需要完整匹配已持久化的 prefix unit；公共前缀也可能在多次请求后才单独落缓存。见 [DeepSeek guide](https://api-docs.deepseek.com/guides/kv_cache/)。
- Gemini 区分自动命中和显式 CachedContent 资源，且 API 家族能力不同。见 [Gemini guide](https://ai.google.dev/gemini-api/docs/caching)、[CachedContent API](https://ai.google.dev/api/caching)。这不是要求本阶段为所有供应商建设原生适配器。

对于 prefix 型缓存，给 B、C 各自取一个内容 hash 不代表删除 B 后 C 的 KV 可独立搬到 A 后面。客户端的分区命名不能改变供应商前缀依赖。

### 任务成本口径

先按供应商口径归一化互斥桶，然后计算：

```text
C_task = Σ(Main 与 Maintenance 的每次真实 attempt)
           [p_uncached * U + p_read * R + Σ_ttl(p_write_ttl * W_ttl) + p_output * O]
         + 显式资源存储费用（如实际适用）
         + 工具执行费用
```

Main/Maintenance 已纳入求和，不再额外把 compaction 事件当作同一笔费用叠加。原始 usage 留存；不同端点不能统一用 input-read-write 推导普通输入。未报告不等于零；reported_model 也不验证网关真实上游身份。

### 为什么“压短”未必马上便宜

仅作数学示例：假设每 token 普通输入价为 1、缓存读取 0.1、写入 1.25。已有 10,000-token 基座命中，考虑缩成 8,000 token，并假设新基座必须全量重写，未来 K 次都能稳定命中，不计压缩模型调用与输出差异：

```text
保留旧基座 = 1,000 K
立即重建   = 8,000 * 1.25 + 800(K-1) = 800K + 9,200
```

K=10 时，分别为 10,000 和 17,200；K=46 相等；K=47 新基座开始略便宜。这不是某个模型的报价，也不是生产应固定 47 轮的建议。若较早断点仍可命中、实际只重写少量后缀，重建成本会更低；TTL、质量、输出和压缩费用也会改变结论。硬失效和资源上限始终优先。

## 7. 三线实施任务与停止条件

### A：执行核心与工具

先调查本 SHA 的 proof_supervision CI 失败，保留确切进程身份与未确认清理事实；不要只加大 timeout 或直接删除测试。并行修 R7 的模型 schema，让搜索新能力真正到达 LLM。

继续现有任务纠正、挂起/恢复、工具执行与验证的真实流程。final packing 对必需证据与角色/协议的检查是缓存方案的硬约束。

**停止条件：** 既有长任务行为仍成立，失败清理有确定结论，正式表面能表达并执行搜索续跑；不为缓存新增平行 task manager。

### B：Context / GC / 搜索

向布局提供可信的来源身份、版本、范围、完整性和 liveness。完成稳定证据/易变状态的投影分离；保留已经修正的 F1/F3。冷元数据继续按现有 F2 的明确规模边界推进。

**停止条件：** 证据不因纯状态变化失去可见性；必要更新即时进入；本地真实内存和持久化保护根可解释；不把“前缀稳定”当作永不释放的理由。

### C：平台与供应商 KV

顺序：R2 路由键与 R3 写策略 → R1/R4 稳定基座边界 → R6 账目完整性 → 有条件的 R5 协议段复用与阶段重建。第一片只完成当前已使用/确认的端点；其余保留能力接口，不阻塞主体。

由一个维护者统一修改 `ModelInput/ModelRequest/PromptReuseBoundary` 契约。其余线按契约提供字段，不分别发明前缀身份、成本状态或完整性定义。

**停止条件：** 实际最终 wire 正确、证据质量不下降、固定任务成本可以解释；不是缓存命中率达到某个未经依据的百分比。

## 8. 验证安排：使用现有测试文件，不另建总门禁

| 场景 | 必须断言 |
|---|---|
| 只改变当前焦点/进度 | 稳定基座字节与可用边界不变；新的指令仍在请求中 |
| 增加 foreground/required miss/恢复正文 | 不在稳定基座前插入易变内容；正文不被隐藏 |
| 同文件不同范围与真实文件改动 | 前者不互相冒充，后者立即失效，保留既有 F3 反例 |
| 工具集合和授权变化 | 最终 schemas 与执行快照一致；撤销不会为命中率延期 |
| 无 hint/过期 hint/压缩请求 | 显式-only 已确认端点不悄悄回到隐式写；未知端点原样 |
| 前一次失败已报 usage，后一次成功或取消 | 两次真实尝试已知账目不丢；同 attempt 的累计事件不重复算 |
| 使用正式 search.grep schema 续跑 | scan_continuation 到达最终 wire 并经 dispatcher 执行下一批 |
| 边界跨多条消息、空消息、工具组 | 合法映射，角色和 call/result 配对不变；非法断点不能强行写入 |

建议验证命令（本轮未执行；在用户开发环境运行）：

```sh
cargo test -p agent-contracts model_cache
cargo test -p provider-openai prompt_cache
cargo test -p provider-openai retry
cargo test -p agent-runtime prompt
cargo test -p tool-runtime search
cargo test -p agent-compose --test kv_cache_walk
cargo test -p agent-compose --test proof_supervision
cargo fmt --all -- --check
```

其中 kv_cache_walk 不加 `--ignored`，仅运行默认离线测试。真实供应商实验必须单独明确预算，不把忽略的付费测试算成已验收。

复用既有 kv_cache_walk/live_walk 与实际跨模块任务，先离线回放最终请求序列，再在明确费用上限内做同起点、同 profile、同验收的对照。热缓存、冷启动、TTL 间隔和缓存命名空间隔离需要记录，避免一个实验臂无意给另一个预热。

输出指标只需：任务验收、真实模型/工具轮次、主/维护每 attempt 的读写/普通输入/输出与缺测、TTFT/总耗时、已有 request/tools/settings 摘要及失效归因。字节前缀相同不是服务端 token 命中，mock wire 通过不是实际省钱。

## 9. 本轮交付边界

本文件补充而不覆盖上一份后端主体计划。已交付的是固定 SHA 的源码发现、更新后的旧项状态、供应商规则约束、三线任务和建议回归。没有把这些建议写回仓库，没有声称新的布局已经实现，也没有声明真实成本节省比例。
