# PLATFORM-2／PLATFORM-4 收口回执：完整结果读取与跨层事实一致（B3＋B4）

基线 `685b6bbb`（阶段基线，本线 PLATFORM-1／PLATFORM-3 同树续接）。涉及工作树**多会话并行落地**：PLATFORM-2 的三个部分（artifact 分页、changes 产物引用、context 驻留四态）由并行平台会话实现，本会话独立评审其设计、修复其跨层缺口、回退自己的重复实现，并完成 PLATFORM-4 的收尾增量。全部代码在工作树（未提交、未推送、未跑远端 CI）。

## PLATFORM-2（F08）：能读完大型产物、从变化摘要定位实际内容、看到信息新鲜度

**① artifact 有界分页（并行会话实现，本会话验证）**：`WorkArtifactRequest` 增加可选 `offset`；`WorkChangeResponse`——即 `WorkArtifactResponse` 返回 `offset`/`truncated`/`next_offset`（cursor 只在截断时存在，eof 不带 cursor；`validate` 强制 `truncated ⇔ next_offset.is_some()`，窗口必须落在工件范围内）。run 绑定与 sealed digest 校验照旧，翻页不可能悄悄换版本。.NET `ReadArtifactAsync(reference, maxBytes, offset)` 镜像同形。

**② changes 读模型可定位的产物引用（并行会话实现，本会话修复 .NET 缺口）**：读取 changes 时，运行时把日志捕获的 before 正文**溢出写入该 run 的 sealed artifact 库**（`change_summary_with_artifact`，捕获上限由日志自身的 256 KiB 预算约束），`ChangeSummary::MutationPrepared` 新增 `old_content_artifact: Option<String>`（`artifact://` 引用）——GUI 用**同一条分页 artifact 路由**读实际内容，不开放任意宿主文件、不建第二套读取通道。日志内部的原始 `old_content` 字段仍然不落 wire。

**本会话修复的真实缺口**：.NET `ChangeSummaryConverter` 的 `PreparedFields` 白名单**未放行 `old_content_artifact`**——真实宿主的 changes 应答会让 .NET 客户端以「unknown field」拒收（deny-strict converter）。已修复并补：`WorkChangesResponse.Validate` 对引用做与 artifact 路由一致的 256 字节 opaque 上限；conformance 测试改为钉住「引用在 wire 上可见、原始 `old_content` 永不可见、无引用时字段整体缺席」三条事实。

**③ context 读模型驻留四态（并行会话实现，本会话验证）**：`ContextItemSummary` 新增 `residency: ContextResidency`（驻留/外部存储——reader 不 fetch 时只见摘要指针）与「本轮实际材料化发送」标记（legacy 序列化 serde default 兼容）。context-simple 的 heap/external 投影如实填充（`heap.rs`：heap 条目 `Resident`、`external_summary` 恒 `External`），协议与 .NET 镜像同形。「在存储中/驻留/本轮实际发送/仅摘要指针」四态由现有事实派生，不建持久化真值。

**设计取舍（如实记录）**：本会话曾独立实现按 `tx_id` 定位的 `work/change` 内容读取路由（agent-workspace `read_change`＋协议 DTO＋测试，工作区 112 项通过）；发现并行会话的 artifact 引用方案后**主动整体回退**——同一需求不ship两套机制，引用方案复用既有分页路由更符合「断链直接接通、不另起通道」。

## PLATFORM-4（B4）：跨层事实与协议一致性

**usage 来源原样读取（本会话实现）**：核心线（CORE-4）在 `RuntimeEvent::ModelUsed` 增加的 `usage_identity`（observed/estimated/unknown）与 `usage` 详细报告，经平台事件通知的 `RuntimeEventEnvelope` 原样转发。.NET SDK 侧按既有设计把事件体保留为原始 `JsonElement`（不建第二套事件代数），但成本事实不应逼 GUI 解析 JSON 字符串——新增类型化访问器：

- `RuntimeEventEnvelope.TryGetModelUsage(out ModelUsageFact)`：从 `model_used` 事件读取 input/output/cached 计数、attempts/retries 与 usage identity；非 usage 事件返回 false（这是关于事件的事实，不是关于消耗的事实）。`ModelUsageFact.IsObserved`/`IsIndeterminate` 强制消费侧区分实测与推计——「丢失 usage 的取消不能显示成零消耗」在 SDK 层有了类型化表达。

**有限跨层 fixture（本会话实现）**：新增共享金样 `event_model_used.json`（完整 notification 帧含 `model_used` 事件与 usage 字段），双侧消费：Rust `work_fixtures::event_fixture_pins_model_usage_facts`（解码为内核类型化信封、断言计数与 `UsageIdentity::Observed`）、.NET `Event_fixture_pins_model_usage_facts`（逐字节 roundtrip＋访问器事实＋非 usage 事件无事实）。

**随附修复的 wire 卫生缺陷**：.NET 信封的四个派生只读属性（`EventType`、两个 `IsLiveOnlyProgress`、通知级 `EventType`）此前会被 System.Text.Json 序列化进输出——一旦客户端侧重序列化事件帧就会注入 Rust 侧不存在的字段。已全部 `[JsonIgnore]`（派生读，永不是 wire 字段），由新 fixture 的逐字节 roundtrip 钉住。

**共享接口版本与错误分类**：ProtocolIdentity 每帧校验（PLATFORM-3 已核）＋fixture 双语言一致性测试既有，本切片补事件面金样后，「关键字段、错误分类、unknown/null」的跨层 fixture 覆盖了全部新事实面。

## 实际执行的检查（本机 Windows，2026-09-11，多会话共享树）

- `cargo test -p agent-platform-protocol`：lib **47**、work_fixtures **10**（含新增 event fixture 测试）全绿。
- `cargo test -p agent-workspace`：lib **108**＋其余 **8** 全绿（我的 `work/change` 实现及其测试已随回退移除，回到并行会话方案的基线）。
- `cargo test -p agent-runtime --test actor`：**81** 全绿（含并行会话的 changes 引用路由测试）。
- `cargo test -p agent-host`：lib 8＋config 3＋e2e **8/8**＋restore 3 全绿。
- `cargo check --workspace --all-targets`：通过；`cargo fmt`（平台四 crate）通过；`cargo clippy`（平台四 crate，all-targets）**0 警告**。
- `dotnet test clients/dotnet/Agent.Client.Tests`：**112/112**（基线 111＋新增 usage fixture 测试）。
- `python scripts/doc_consistency.py`：OK（13 live docs）。

## 未验收 / 边界（如实记录）

- 未提交、未推送、未跑远端 CI；「关闭」待 CI run 记录确认。
- PLATFORM-2 三个子部分的主体实现出自并行平台会话：本回执对其设计做了独立评审（分页 cursor 语义、引用方案 vs 内容内联、驻留四态派生来源），修复了其 .NET 侧一处会使真实宿主流量拒收的缺口，并执行了上列全部验证；**最终联合验收由本回执与该会话的记录共同构成**。
- GUI 消费（GUI-2 结果审阅工作台、GUI-4 成本面板读 `TryGetModelUsage`）归 C 线。
- `residency`/「本轮发送」是引擎与装配侧的派生事实，不改变任何 GC/选择语义；context-simple 的改动仅投影、未触打分与驻留策略。
- 真实 provider 场景照旧 NOT_RUN；多客户端交错与 Linux UDS 全量由 CI 承接。
