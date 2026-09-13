# C 线实施回执：C1（N04 KV 键与多断点到 wire）＋ C2（N05/N06 空集与边界验证）

日期：2026-09-14。基线 `6eda2474`；依据 [REVIEW_6eda2474.md](REVIEW_6eda2474.md) 与 [NEXT_BACKEND_TASKS.md](NEXT_BACKEND_TASKS.md) 的 C 任务包。C3（条件成本对照）未执行，见文末。

## C2（N05/N06）：空集显式声明＋split 结构验证先于切片

- **N05（`agent-runtime/src/prompt.rs`）**：assembler 改为按**类型化装配计数**声明 split——`context_frame[0]` 是 SELECTED WORKING CONTEXT 块当且仅当 `history.items` 非空，故新输出**恒为 `Some`**：有选中集 `Some{base:0, epoch:1}`，空选中集显式 `Some{base:0, epoch:0}`。`None` 只留给确实缺少新声明的旧序列化输入（into_request 保留其整帧前缀兼容）。**内容不再决定结构**：文件正文含 "SELECTED WORKING CONTEXT" 字面量不再能触发/改变分区。
- **N06（`agent-contracts/src/model.rs::into_request`）**：声明的 base/epoch 在任何切片之前验证——`checked_add`＋`base+epoch ≤ context_frame.len()`；非法声明**放弃缓存 hint**（前缀收敛到稳定 system policy、无断点），消息/角色/工具配对完整保留，绝不冒充旧格式（不给非法输入整帧前缀奖励）、绝不把动态区声明为稳定区。

回归（runtime prompt 3 项、contracts 2 项，均红检查在先）：空选中集＋volatile foreground → 显式 `{0,0}`（旧代码为 None，红）；正文含标题字面量 → split 不变（旧代码误触 `{0,1}`，红）；非法 split（usize::MAX 溢出 / 超层 / 空组合）不 panic、消息逐条不变、无断点、边界不越 volatile 帧；恰好整帧的合法 split 保持断点 `[1,2]` 与边界绑定。

## C1（N04）：key 与多断点真正到达生产 wire

- **契约**：`PromptCacheRouting { isolation, workspace, endpoint }`——组合根的稳定路由命名空间；`key_for(task, lane)` 派生不透明键 `{isolation}|{workspace}|{endpoint}|{task}|{lane}`（组件 ≤256 字符、超长摘要化；总 ≤1024）。**不是每轮 UUID、不是请求 digest**；同任务的轮次与恢复间稳定，跨隔离域不同。
- **Runtime 生产调用点**（`actor/model.rs`，唯一的 `into_request` 生产消费点）：main lane 请求以 `{routing}|{task_id}|main` 填 `prompt_cache_key`；无路由配置保持 keyless（未知能力端点/裸组合历史载荷不变）。
- **维护 lane**：`ModelBackedCompactor::with_cache_key`——组合根注入 `{routing}|compaction|maintenance`；维护请求的 `cache_write_policy: ExplicitOnly` 既有声明保留。
- **compose 接线**：`ComposeConfig.cache_routing` → services（main lane）；host main / TUI main 从 provider profile（base_url）＋workspace canonical root 组装路由并派生维护键传入 `build_context_engine`。mock/demo 组合保持 keyless。
- **Responses mapper 多断点映射**：`request.cache_breakpoints`（B0=stable policy 末、B1=declared epoch 末，均为 last-non-empty 平铺下标）**逐项**映射到 wire input——空消息过滤与 assistant 工具调用项展开造成的下标漂移由「每消息产出的最后一个 wire item」归属解决；内容项保持既有的 content 内标记形状，展开项（function_call/function_call_output）以同级 `prompt_cache_breakpoint` 字段标记；仅 `ResponsesExplicit` 已确认能力端点发送（未知能力端点载荷逐字节不变）；legacy 单边界 hint 在无声明列表时保留原行为。

## 验收：真实生产链路（非 DTO 手填）

`crates/agent-compose/tests/cache_wire_flow.rs`——真实 `OpenAiProvider`（Responses 协议＋`ResponsesExplicit`、no-proxy 客户端）指向本地捕获服务器，`compose(ComposeConfig)` 全链启动，`set_focus`＋两轮 user message（round 1 发起**真实 fs.write 工具调用**）：

- 同任务全部请求 `prompt_cache_key` 逐字节相同、非空、含 isolation/`|main`、不含请求正文；
- `prompt_cache_options.mode == "explicit"` 在 wire 上；
- round 1（空选中集）wire 上**只有 B0**——N05 语义在生产 wire 的直接体现；fs.write 证据产生后的轮次 **B0 与 B1 都在**、B0 先于 B1；
- 服务器按请求脚本返回真实 fs.write 工具调用，工具经 builtin 分发器实际落盘（round 2 的证据来自真实工作区）。

期间顺带验证：R3-12 的失败分支 Unknown 行在真实链路（malformed SSE 触发的失败轮）正确发射。

## 验证汇总

`cargo test -p agent-contracts --lib` **178**、`cargo test -p agent-runtime --lib prompt::` **50**、`cargo test -p provider-openai --lib` **147**、`cargo test -p agent-compose --test cache_wire_flow` 1/1、`cargo fmt --check`（六 crate）通过、contracts/provider clippy **0** 警告。

## 边界与未验收（如实记录）

- **端点收字段确认**（"仅确认端点收字段"）：本地捕获服务器证明 wire 形状与存在性；真实 DeepSeek/OpenAI 端点对 `prompt_cache_key`/多断点字段的接受度归 C3 的有界真实执行窗口（本轮未调用付费模型）。
- 成功路径上更早失败尝试的已知计数仍无逐尝试事件通道（沿第三轮回执残余）。
- hydrate_all 总工作量沿 B3/原队列明确支持规模，不在本片。
- C3 未执行（条件任务）：无预算/凭据指令时记 NOT_RUN；实测前不报降本百分比。
- 未提交/推送、未跑远端 CI。共享树 A/B 线第三轮在飞（tool-runtime/session.rs、mcp.rs 窗口间编译中间态），本片全部验证在可编译窗口完成。
