# GUI-2 实施回执：artifact 分页垂直切片＋审阅工作台消费＋输出/日志分离

- 日期：2026-09-11（工作树，基线 `685b6bbb`，未提交）
- 对应缺陷：2026-09-11 审查 **F08（P2 → PLATFORM-2＋GUI-2）**；切片归属 [NEXT_TASKS.md](../../NEXT_TASKS.md) **B3（PLATFORM-2）artifact 分页半** 与 **C2（GUI-2）**。
- 执行方式：**重叠切片一次执行、双边关闭**——GUI 线在平台线无在途 artifact 改动的窗口内，按任务书修复方向把 PLATFORM-2 的 artifact 分页半（协议/运行时/.NET DTO）与 GUI-2 消费面一次做完；`changes` 读模型与 `context` 读模型的扩展由平台线并行落地中（本片未触碰）。核心线文件零改动。

## 用户能做什么

大产物的尾部关键结论可以从 GUI 独立核查：**读取**（从字节 0 开始）、**下一页**（沿服务端游标续读，eof 时按钮失效）、**读尾部**（探测大小后直跳最后一个窗口）。每个工件窗口都带身份与完整性标记（引用、总字节数、当前窗口字节区间、eof/后续还有、每页上限）；页边界切开的多字节字符在下一页自动补全，不渲染替换符。**输出正文与运行日志分开呈现**：模型增量正文独占「输出正文」面板，受理回执/失败/账目/事件行进入「运行日志」面板——日志不再冒充任务结果真值。重复查看只是只读 wire 调用，不重触发任何模型或工具副作用。

## PLATFORM-2（artifact 分页半）wire 变更

- `WorkArtifactRequest` 增 `offset: Option<u64>`（缺省/0＝既有前缀读）。
- `WorkArtifactResponse` 增 `offset: u64`（serde default，窗口起始字节）与 `next_offset: Option<u64>`（恰在 `truncated` 为真时出现，指向首个未读字节）；`truncated` 语义由「前缀被截断」一般化为「本窗口之后还有字节」。
- 双侧校验器一致收紧：窗口必须落在 `[0,size]` 内；`truncated == next_offset.is_some()`；truncated ⇒ `next_offset == offset+decoded` 且未达末尾；非 truncated ⇒ 窗口恰好到达末尾。旧语义（offset=0 的完整读/截断前缀读）作为特例完全兼容。
- 运行时 `WorkControlRouter::artifact`：按 offset seek 后读 `min(max, size-offset)` 字节；offset 越界返回结构化拒绝（不钳制、不假装）。sealed artifact 不可变，逐页重复同一身份核验——**分页不可能悄悄换版本**。
- 宿主 e2e（`host_e2e.rs`）扩为分页序列断言：tiny 首页游标＝首个未读字节；沿游标逐页重组与整读逐字节一致；末页 eof 无游标；越界 offset 被拒。

## GUI-2 消费面

- 分页状态：有界累积窗口（`MaxArtifactWindowBytes`＝1 MiB，超限释放最旧字节且**面板明示**释放，从不在 UTF-8 序列中间切）；`ArtifactNextOffset` 驱动「下一页」命令可用性；断开连接整体作废会话（窗口/游标/身份）。
- 增量 UTF-8：页边界切分的多字节字符由 `IncompleteUtf8TailLength` 识别并暂扣显示（字节保留在窗口），下一页补全；eof 处按原样解码。
- 「读尾部」＝一次 1 字节探测拿 size，再直读最后窗口——两个只读调用直达尾部，不逐页翻。
- 面板头部：`工件 {ref}：{size} 字节 · 窗口 [start,end) · {eof | 后续还有（下一页 N）} · 每页至多 65536 字节`。
- 输出/日志分离：`AppendOutput` 只承载模型 delta 正文；新增 `AppendLog`（同等有界保留）承载全部回执/失败/账目/事件行；AXAML 改为「输出正文／运行日志」双 Tab。

## 回归

Rust（先于并行线 changes 半的窗口内全绿）：
- 协议 `artifact_pages_carry_offset_and_continuation_truth`（中间页游标、eof 无游标、跳字节/倒退/越界/缺游标/游标悬空全部拒绝）＋既有截断真值测试随新 wire 更新——47/47；
- `cargo check -p agent-runtime` 通过；`cargo test -p agent-host --test host_e2e` **8/8**（含分页重组；首跑一次负载抖动失败、复跑全绿，属既有记载类型）；
- `cargo fmt` 通过。

.NET（111/111）：
- 新增 `Artifact_read_shows_identity_window_and_eof_facts`（身份/窗口/续读标记＋初始读 offset=0）、`Artifact_pages_continue_to_eof_and_reassemble_multibyte_text`（8 字节页逐页续读到 eof，多字节字符跨页重组无替换符）、`Artifact_tail_read_jumps_to_the_last_window`（探测＋直跳末窗，恰好两次调用）、`Model_deltas_fill_the_output_panel_while_receipts_fill_the_log`（正文/日志互斥）；
- 既有测试随行为迁移：运行日志类断言（已受理/审批送达/终态/工具事件/账本核对等）从 `OutputText` 迁到 `LogText`（RestoreWalkthrough/Integration/HostChain/Lifecycle/ContinueCancel/F06）；命令注册计数基线 9→11（新增两个分页命令）；`FixtureConformanceTests` 的 artifact DTO 用例随新 wire 补 `NextOffset`；`WorkbenchIntegrationTests` 计数 9→11。

## 实际检查

- `cargo test -p agent-platform-protocol --lib` 47/47（在并行线 changes 半合入前的窗口内）；`cargo check -p agent-runtime` 通过；`cargo test -p agent-host --test host_e2e` 8/8；`cargo fmt --check`（三 crate）通过。
- `dotnet test` **111/111**；`dotnet build apps/Agent.Desktop` 0 警告 0 错误。
- 回执写作时并行平台线正在同文件落地 changes 读模型半（`old_content_artifact`），其测试字面量当时未收口——非本片内容，未触碰；本片 artifact 部分在合入前的窗口内已独立验证。

## 未验收（如实记录）

- 未提交、未推送、未跑远端 CI；`changes` 读模型与 `context` 读模型扩展（PLATFORM-2 另外两半）由平台线并行落地，本片未验证其状态。
- 真实宿主上的分页端到端未在本机重跑（host_e2e 已含真实宿主分页序列；GUI→真实宿主的分页人工走查未做）。
- 「点击证据可回到对应调用或结果」（GUI-2 深链）未做——changes 行已携带 tx/路径/工件引用等类型化字段，深链属后续 UI 增量。
- 真实 provider 场景照旧 NOT_RUN。
