# G2/G3 回执 — 最终模型预算内的连续交付（artifact.read ＋ fs.read）

提交：`d1d5c8dd`（含 fs.read 扩展）。任务规格：[NEXT_ACTIONS.md](NEXT_ACTIONS.md) G2/G3；缺陷分析：[REVIEW.md](REVIEW.md) 第 3–4 节。上轮 F1（动态 `end_line`）、F2（`line_byte_offset` 行内游标）、F6（coverage footer）保留并组合。

## 用户动作

原样使用工具返回的 continuation 读完原始工件／文件，不跳过中间行／行内后缀，不把真实第 3 行显示成第 2 行；结束声明有真实依据。

## 实现 — 一条规则一个生产入口

新增 `tool-runtime/src/tools/page.rs` 作为共享分页机制（两条读工具路径的同一维护入口）：

- `FINAL_BODY_CHARS = agent_contracts::MAX_TOOL_MODEL_CONTENT_CHARS`（16,000，即经纪裁剪上限 `agent-contracts/src/tool.rs:13`）——capture 循环与 `ToolSpec::output_budget` 共用同一定义（kernel 把 `spec.output_budget` 传给经纪，工具按它分页，经纪不再需要剪页中）。
- `finalize_within_budget(spans, render)`：渲染**最终**包络（各工具经闭包提供 header/span/footer）、按预算度量、超限时丢弃尾部 span 并返回保留集；游标与交付声明**只从保留集推导**——游标永远不会越过未交付源位置。`artifact.rs` 与 `fs.rs` 都经由它。
- `DeliveredSpan` 携带真实源行号（重编号结构性不可能）；`rendered_prefix_chars` 按实际前缀宽度计费（修正旧固定 8 列对 7＋位行号的低估）。
- 预留与真实渲染同源：footer/header 预留用**同一个**子句构建器以最坏情况数字计算，预留不可能漂移；按渲染**字符**记账（与经纪裁剪口径一致），多字节字符按码点完整放置。

扫描位置（`ScannedPosition{lines, bytes, complete}`，仅总量）、捕获位置（capture 循环 span）、最终交付位置（`CoverageFacts::next`）三种位置分别表达；metadata 携带 `positions.scanned/captured/delivered`，legacy `next_start_line`/`next_line_byte_offset` ＝交付位置。

G3：capture 在**第一个不可展示位置**关闭（余量放不下一个完整码点、或行内切割后 `take==0` 即 `capture_closed`），不越过空洞接纳后续行；行号绑定源位置；分隔符/换行/行号成本同一账本。`window_truncated` 诚实化为"交付页未覆盖窗口∩已扫描范围"。

fs.read 扩展（KV 轨迹证实同一缺陷形态存在于 `fs.read`）：按最终预算整行分页；`lines=S-E/N` 声明只描述**实际交付**区间（窗口装得下时与请求完全一致，既有行为全保留）；新增 `has_more`/`next_start_line` metadata 与正文 `continue with fs.read …` 文件游标（窗口内先走完窗口、之后按文件 200 行页推进，`has_more=false` 仅在真 EOF）；整页预算都放不下的超长行以正文显式声明跳过（"line N exceeds the page budget and is not shown"），不静默、不半交付。`agent-workspace` 经纪未改——仍是不当生产者的兜底。

## 回归（红→绿，先只有测试代码对未修改生产逻辑运行）

artifact（`tools::artifact` 测试）：

- `g2_probe_a_every_line_delivered_exactly_once_via_returned_continuations` — 500×150 字符行、每行唯一 ID，真实 tool→broker 路径只按返回 continuation 走，交付 span 重建全文逐字相等（无缺口/无重叠），显式覆盖第 100/300 行。修复前红：`delivered source intervals must cover lines 1..=500` — left 缺 51..153、251..353、451..452。
- `g2_probe_b_three_mib_single_line_block_ids_all_delivered` — 3 MiB 单行、1 MiB/2 MiB/2.5 MiB 唯一块 ID，`line_byte_offset` 游标走完，各恰好交付一次，页间区间数学证明 `[prev, next)` 字节精确。修复前红：`block G2B-BLK-1M-c31f must be delivered exactly once: left: 0, right: 1`。
- `g3_multibyte_gap_captures_no_line_after_the_first_unshowable_position` — 审查反例原形（余 1 字节、次行"界"、后随短行）：第 1 页必须 `has_more` 且不在第 1 行后捕获任何内容；后续行按真实行号 2/3 交付、真 EOF。修复前红：`line 2 is unshown: the page must not claim completion — left: false, right: true`（旧代码报 `has_more=false`）。
- `g3_exact_room_closes_capture_at_the_unshowable_line_start`、`finalize_falls_back_behind_the_overrunning_spans`（包络超限安全网直测）；walk helper 断言每页 ≤16,000 字符且不含经纪截断标记。

fs.read（`tools::fs` 测试，红例对未修改 fs.read 运行：`19 passed; 4 failed`，四例同报 `a page under the final budget must reach the model verbatim — no head+tail clip`）：

- `fs_read_walk_delivers_every_line_via_returned_continuations` — 600×150（KV 形状）走真实 tool→broker、只按返回 continuation、每行逐字恰好一次、第 100/300/500 行标记交付、无截断标记。
- `fs_read_claim_and_metadata_describe_the_delivered_range` — `lines=S-E/600` 声明＝渲染条目区间＝metadata start/end；`next_start_line`＝交付尾＋1。
- `fs_read_multibyte_pages_by_chars_without_cutting_code_points` — 300 行"界"按字符整行分页，无 U+FFFD，真实行号，walk 收敛。
- `fs_read_oversized_line_is_declared_not_silently_skipped` — 40k 字符行正文显式声明，第二行按真实行号 2 交付。

既有 F1/F2/F6 测试全部保持（两处超大 fixture 按新页幅缩小、目的不变；3 MiB probe 与 legacy-cap G3 probe 保持全尺寸）。fs 既有 19 测未改动全绿（400 行换行密集窗口渲染约 4.4k 字符装得下预算，窗口装得下时 metadata start/end ＝请求值）。

## 已执行验证（Windows，cargo 1.97.1，合树后集成复跑）

- `cargo test -p tool-runtime`：**299 passed / 0 failed / 1 ignored**（九批基线 290 ＋ 9 净新增）。
- `cargo test -p agent-workspace`：117＋5＋3 passed / 0 failed（生产未改）。
- `cargo test -p agent-conformance`：全绿（下游只读核对）。
- `cargo test -p agent-compose --test core3_restore_snapshot_paging`：1 passed（restore 后 artifact continuation 不断）。
- `cargo clippy -p tool-runtime / -p agent-workspace --all-targets -- -D warnings`（过滤本片文件 0 诊断）；workspace 级 fmt/clippy 在五片合树后全干净（见集成段）。

## 下游与边界

- KV 生产序列（`kv_production_sequence.rs`）的 EXPECTED-RED 交付签名消失；walk-shape 钉子重校准见 [KV_SEQUENCE_COMPLETENESS_RECEIPT](KV_SEQUENCE_COMPLETENESS_RECEIPT.md)。
- fs.read 超预算场景下 metadata `start_line/end_line` 语义改为交付区间（装得下时不变）；下游 `prompt.rs::FileBodyWindow` 读取该字段——精确度保持或提高；经纪 E1 剪裁戳仍是任何残余裁剪的兜底（本两工具已无页中裁剪）。
- 未验证：真实供应商端点的缓存效果（超范围）；GUI/eval 面未触碰。
