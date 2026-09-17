# H3/H5 回执 — 真实源结束与真实文本交付

**提交：H3 `766b4136`、H5 `56312531`**（第十一批，基线 `d3a05d29`，审查：[REVIEW.md](REVIEW.md)）。两片独立成型、分开提交；实施环境 Windows＋cargo 1.97.1，红例先行。

## H3 — 恰好 8 MiB 有真实 EOF 证明（crates/tool-runtime/src/tools/artifact.rs）

- `limit() > 0` 的预算猜测改为：预算耗尽时经 `reader.get_mut().get_mut()`（take 内层同一 File 句柄，扫描后恰停在扫描起点＋scanned_bytes）做**有界 1 字节探测**——读到 0＝真实 EOF→complete；读到 1＝真实后缀→BudgetStop 不完整；读错误保守判不完整。探测不占扫描预算、不进 capture（L548–563）。
- 既有 `ScannedPosition.complete` bool 保留、只变诚实；footer/`has_more`/续读游标/summary/metadata 全部由它派生——F1/F2/G2/G3 已收口语义未动。`SCAN_BUFFER_BYTES = 1 MiB` 顺序扫描缓冲（L306、L424–425）替换默认 8 KiB，纯缓冲语义（8 MiB 走查测试在 debug 构建下可行的前提）。

红→绿：修复前 cap（8388608）报 `left: Bool(false), right: true`，单行走查 900 页不收敛（start_line=201/401/601… 无限重扫，红跑实测 407s）；修复后 16.4s 全绿。回归（真实临时文件）：`scan_completion_at_the_cap_boundary_reflects_the_source_not_the_budget`（cap−1/cap/cap+1 × 有/无结尾换行 × 单行/多行 12 组合）、`cap_sized_single_line_terminates_through_returned_continuations`（8 MiB 单行经真实 broker 按返回 continuation 原样连读 ~530 页到 `end of artifact (1 lines)`、`has_more=false`）、`cap_sized_multiline_artifact_terminates_through_returned_continuations`（末行无换行）、`one_byte_past_the_cap_keeps_honest_incomplete_totals`（cap+1 如实不完整、绝不宣称真实结束）。

## H5 — 跨片合法 UTF-8 不再失真（crates/tool-runtime/src/tools/stream.rs）

- `Utf8Tail`/`decode_utf8_incremental`/`flush_utf8_tail`（191/221/273）：按 `from_utf8` 的 valid_up_to/error_len 语义增量解码——完整序列照常输出、非法序列恰一个 U+FFFD 并消费、≤3 字节未完成尾缀按流保留；EOF 按 lossy 语义冲刷。
- `StreamCapture.decode: [Utf8Tail; 2]`（293）：stdout/stderr **各自独立**解码状态，禁止跨流拼字；`record`（312）解码只影响 model tail——**原始工件逐字节写入、字节统计、channel/背压/取消语义全部不变**；newline 边界即刻收尾（换行不可能补全跨行序列）；EOF marker 分支（329）仅在冲刷出文本时计数＋入 tail，omission 算术保持精确。
- `pump_stream`（L157–160）每流结束发一个空 `eof=true` 标记（`OutputChunk.eof` 新字段）；shell/process/session 调用点零改动。

红→绿：修复前拆分 4 字节 emoji 产出 2 个 U+FFFD、合法 9KB 流 8 个 U+FFFD、交错流 6 个 U+FFFD；修复后零。回归（全部走真实 pump＋capture 路径）：`multibyte_chars_split_at_the_item_boundary_reach_the_model_tail_intact`（2/3/4 字节字符 × 起点 3997–4001 共 15 组）、`no_newline_eof_flushes_the_retained_suffix_lossily`、`stdout_and_stderr_decode_suffixes_stay_independent`（错峰交错＋逐流字节重建）、`invalid_utf8_fragments_replace_exactly_the_invalid_sequences`（[0xff,0xfe]→恰 2 个 U+FFFD）、`legal_multibyte_streams_stay_replacement_free_end_to_end`（相位偏移使 4000/8000 边界落在 4 字节字符内部＋无换行 EOF）。

## 命令与结果（实际执行）

- `cargo test -p tool-runtime --lib`：**308 passed / 0 failed / 1 ignored**（基线 299/0/1＋新增 9；41–43s，与基线持平；集成人复跑 40.78s 同结果）。
- 定向：`tools::artifact` 24/0（旧 20＋新 4）、`tools::stream` 7/0（旧 2＋新 5）。
- `cargo clippy -p tool-runtime --all-targets -- -D warnings`：通过；`cargo fmt -p tool-runtime` 已执行。
- 集成终验：compose/conformance/workspace 全套 0 失败（经纪与最终正文链路未回归）。

## 剩余限制（如实）

- 未做 release 性能测量，不声称收益。每页 ~220ms 实测成本主要在 sealed 工件的既有 digest 复验（~197ms/8 MiB，完整性设计未放宽）；走查测试用 draft 定位符避免把哈希时间误计入扫描语义。
- 超预算工件每页仍从字节 0 重扫同一前缀（既有 W06 语义，未扩权）；探测为每个耗尽预算的页增加一次单字节读。
- 无换行 EOF 处止于字符中间的片段，冲刷出的替换文本作为独立 tail 条目呈现；原始工件字节不受影响。
