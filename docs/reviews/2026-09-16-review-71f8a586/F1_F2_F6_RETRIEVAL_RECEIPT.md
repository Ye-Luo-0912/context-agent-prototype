# F1/F2/F6 取回切片回执：可继续且不漏内容的工件读取与符号扫描覆盖

- 分支：`f126-retrieval`（worktree `cap-ret`）
- 提交：`33d797d5`（F1）、`8053c3b0`（F2）、`a6bd208c`（F6）、文档/回执提交见文末
- 范围：`crates/tool-runtime/src/tools/artifact.rs`、`crates/tool-runtime/src/tools/code.rs`、`docs/TOOL_INVENTORY.json`（注记同步）
- 依据：[REVIEW.md](REVIEW.md) F1/F2/F6、[NEXT_ACTIONS.md](NEXT_ACTIONS.md) §1
- 未动：`docs/CURRENT.md`、`docs/NEXT_TASKS.md`、E1 经纪规则、E2–E4、O1/O2、搜索 grep 的既有 cursor/scan_continuation 机制

## 用户动作

按工具给出的参数一路读取工件（含超长单行尾部）；符号搜索不完整时，执行者 LLM 能从模型正文知道未覆盖范围。

## 源码核对（先于动手）

REVIEW 三项描述与分支代码一致，均已完整读取确认：

- F1：`ArtifactReadArgs` 的 `default_end_line()=200` 与 `start_line` 独立默认；coverage footer 只建议 `reference + start_line=next`；照做得到 `end_line(200) < start_line(201)` → `invalid line range`。
- F2：截断分支只存前缀＋补换行，但 `last_captured_line = counted_lines`（整行标记已处理）；`has_more = !scan_complete || in_window_unshown || beyond_window` 不含行内未展示后缀；3 MiB 单行（扫描预算内）得到 `scan_complete=true`、`counted_lines=1`、`has_more=false`、正文 `end of artifact`。
- F6：`scan_incomplete = walk_incomplete || symbols.len() >= limit` 只进 metadata；空命中正文是 `no symbols found`，非空只有符号行＋结果工件分页 note，全程无 coverage footer。

经纪层核对：`MAX_TOOL_MODEL_CONTENT_CHARS = 16_000`（工具自身 2 MiB 输出必被 `WorkspaceOutputBroker::bound` 以头/标记/尾预览裁剪）；footer 位于正文最后一行，落在经纪保留的尾部，续读参数在经纪截断后仍可提取执行（红例即在 brokered 正文上提取）。E1 的 `invalidate_file_read_window_after_body_clip` 只作用于声明 `start_line/end_line/covers_file` metadata 的输出（fs.read 窗口），artifact.read 不发这些键，规则未被触碰。

## F1：续读建议被自己的默认参数拒绝

修复（审查建议 a）：`end_line` 缺省时按 `start_line` 派生有界页窗 `start_line..start_line+199`（`DEFAULT_PAGE_LINES=200`，`checked_add` 溢出时干净拒绝 `invalid line range: start_line leaves no room for a page`）。生成端（footer 只给 `start_line`）与解析端共享同一类型化语义，不靠提示词。显式 `end_line` 的旧语义与 EOF clamp 不变；`start_line` 超过总行数从旧的 `invalid line range` 错误改为诚实的空页＋`end of artifact (N lines)`；纯默认读取（1..=200）与旧行为逐字节一致。

## F2：超长单行的行内字节游标

修复（不发明第二套工件身份）：新增 `line_byte_offset: Option<usize>` 参数——`start_line` 行内的**原始字节偏移**，身份绑定由既有 `reference`（artifact locator）承担。要点：

- 只有实际展示到的位置推进游标：截断分支在**原始字节**的 UTF-8 码点边界上切割（新增 `is_char_boundary(&[u8])`，因 `str::is_char_boundary` 不可用于字节切片），游标记录 `skip + chunk.len()` 原始字节；渲染与游标共用一套坐标系。
- lossy 渲染膨胀（无效字节 1→3）时回退到 room/3 原始字节，数学上保证渲染串不超声明上限；按渲染字节计预算的旧行为保留。
- 行终止符是结构不是内容：`content_len = raw_len - eol_len`（CRLF=2/LF=1/无=0），只剩 EOL 未展示不算未展示区间，不发明续读；`offset >= content_len` 或非边界偏移干净拒绝。
- 截断发生后同窗口后续行保持未展示：续读从行内游标处按序补发，不跳过、不重复；`has_more`/`window_truncated`/`end of artifact` 与正文一致——`end of artifact` 只在没有任何未展示区间时出现；footer 在行内续读时给出 `continue with artifact.read reference=X start_line=N line_byte_offset=M`，并注明该行在捕获上限处被截断。
- 经纪层串联：3 MiB 单行页（约 2 MiB 正文）经 broker 裁到 16k 头尾预览后，footer 续读参数仍可从 brokered 正文提取并继续执行（红例即按此路径断言）。

## F6：code.symbols 部分扫描进入模型正文

沿既有 `coverage_footer` 原语（F04/CORE-3），`scan_incomplete` 时在 `model_content` 尾行区分四件事：遍历停止原因（`the candidate file walk stopped at its cap; files beyond it were never searched`）、结果上限停止原因（`the scan stopped at the N-symbol result cap; …`，区分「剩余候选文件未搜索」与「文件内未收集」）、已扫描范围（`partial scan: {scanned} source files searched, {unread} unreadable or oversized files skipped`）、显示上限（`the rows above are the first K of N symbols (display cap); the full list is in the artifact reference above`）。没有真实扫描续跑实现，不发 scan continuation，给诚实下一步（`narrow the scan with a path subdirectory or a query filter`）；已有结果的工件分页 note 原样保留。小型完整扫描输出语义不变（无 footer；空工作区完整扫描仍是朴素的 `no symbols found`）。

## 回归（红先；F1/F2 用变异恢复法复验承重）

全部红例经 `Tool::execute` → 真实 `WorkspaceOutputBroker::bound` → **brokered model_content** 上断言；续读测试从正文提取工具自己返回的参数**原样**回放，不替产品补参数（提取器 `continuation_args`）。

F1 新增：

| 测试 | 要点 |
|---|---|
| `the_returned_continuation_is_executable_verbatim_until_the_sentinel` | 520 行工件，逐页 verbatim 续读到 line-500 sentinel 与 line-520 末行，游标单调 |
| `defaulted_end_line_stays_bounded_and_rejects_overflow` | 裸 start_line 得 2..=201 页窗；越界 start 是诚实空页；`usize::MAX` 干净拒绝；单行文件默认读保持朴素正文；重复读不重复计数 |

F2 新增/重写（7 条）：

| 测试 | 要点 |
|---|---|
| `long_first_line_truncates_at_the_cap_without_merging_lines`（重写） | 旧行 `next_start_line=2` 改为 `next_start_line=1 + next_line_byte_offset>0`；verbatim 续读先见首行 suffix sentinel 再见 tail-0..99；单行渲染不拼接、预算不超；显式 start_line=2 跳过仍是合法动作 |
| `a_truncated_long_line_is_resumable_to_real_eof_through_the_broker` | 3 MiB 单行、sentinel 在后半部分；老代码在首页就报 `end of artifact` 且 `window_truncated=true`（红）；修复后 2–3 页到达 sentinel＋真实 EOF |
| `the_truncated_first_line_suffix_is_shown_before_the_second_line` | 首行截断后先补 suffix 再到第二行（老代码建议 start_line=2 直接跳过：红） |
| `a_crlf_long_line_resumes_and_reaches_the_next_line` | CRLF 终止符按结构处理，total_lines=2、真实 EOF |
| `a_multibyte_line_resumes_on_character_boundaries` | 「界」3 MiB 单行，任何页无 U+FFFD，原始偏移单调，sentinel 可达 |
| `the_byte_cap_boundary_does_not_invent_a_continuation` | 内容恰等于捕获上限：只有 EOL 被切，无续读、朴素完整页；+1 字节：行内游标指向 cap 处，一步到真实 EOF |
| `invalid_utf8_expansion_stays_within_the_capture_cap` | 0xFF×3 MiB：页串不超 `MAX_READ_BYTES`，游标按原始字节推进到真实 EOF |

F6 新增（4 条，3 红 1 对照）：

| 测试 | 要点 |
|---|---|
| `an_incomplete_walk_reports_the_partial_scan_in_the_body` | 5010 个候选文件使 walk 停在 5000 上限、空命中：brokered 正文含 `[coverage]`＋`never searched`＋`no scan continuation`（老代码只有 `no symbols found`：红） |
| `a_result_cap_stop_names_the_unsearched_candidates_in_the_body` | limit=10 单文件 250 符号：正文含 `result cap`＋`not collected`（红） |
| `the_body_separates_display_cap_from_scan_coverage` | limit=150＋显示上限 100/150：`50 more symbols`（显示）与 `result cap`（覆盖）并存，且无 `never searched`（红） |
| `a_small_complete_scan_keeps_its_plain_output` | 对照：小型完整扫描无 footer、空工作区完整扫描恰为 `no symbols found`（红先绿后均应成立） |

**变异恢复法**（改后立即还原）：

- F1：把派生还原为固定 `None => 200usize` → `the_returned_continuation…` **FAILED**（`InvalidRequest("invalid line range")`）→ 还原后 20/20 绿。
- F2：禁用 `mid_line_cursor` 推进（`captured_truncated = true` 但不记游标）→ 7 条 F2 测试全部 **FAILED**（13 passed / 7 failed）→ 还原后 20/20 绿。
- 环境注记：共享 `CARGO_TARGET_DIR` 下曾出现两次还原后仍红的假象，根因是 `mv` 还原的文件 mtime 早于缓存编译产物、cargo 未重编；`touch` 源文件后重跑即绿。两次（变异红/还原绿）结果均已按真实编译结果记录。

## 已执行验证（真实命令与结果）

```
cargo test -p tool-runtime artifact   → 20 passed; 0 failed
cargo test -p tool-runtime code       → 14 passed; 0 failed
cargo test -p tool-runtime            → 289 passed; 0 failed; 1 ignored（整 crate）
cargo test -p agent-conformance       → 16+11+5+3 passed（lib+builtin+其余 target 全绿；
                                        builtin 含经真实 broker 的 artifact.read envelope 用例）
cargo fmt --all                       → 仅格式重排（已并入提交）
cargo clippy -p tool-runtime --all-targets → 0 警告 0 错误
python scripts/doc_consistency.py     → OK（13 live docs；TOOL_INVENTORY.json 同步后 JSON 校验通过）
```

红/绿对照（同一命令 `cargo test -p tool-runtime artifact --lib` / `code --lib`）：

- F1：修复前 13 passed / **2 failed** → 修复后 15 passed / 0（后随 F2 增至 20）。
- F2：修复前 13 passed / **7 failed** → 修复后 20 passed / 0。
- F6：修复前 11 passed / **3 failed** → 修复后 14 passed / 0。

## 代码位置

- `crates/tool-runtime/src/tools/artifact.rs`：`ArtifactReadArgs`（end_line Option＋line_byte_offset）、`DEFAULT_PAGE_LINES`/`CAPTURE_CAP`/`is_char_boundary`、`execute`（派生校验、捕获循环截断分支、mid_line_cursor、has_more/next 计算、coverage footer、metadata）
- `crates/tool-runtime/src/tools/code.rs`：`execute`（examined/unread 计数、`collected_files`、coverage clauses、`with_coverage_footer`）
- `docs/TOOL_INVENTORY.json`：artifact.read schema/limits/output 注记、code.symbols limits 注记

## 提交

- `33d797d5` tool-runtime: f1 — artifact.read's own continuation suggestion is now executable verbatim …
- `8053c3b0` tool-runtime: f2 — a mid-line capture cut no longer fakes the end of an artifact …
- `a6bd208c` tool-runtime: f6 — code.symbols' known partial scans enter the model-visible body …
- 本回执＋TOOL_INVENTORY 注记：见 `git log` 最新 docs 提交

## 限制（如实）

- 未运行：`agent-compose` 全套（含 proof_supervision、KV wire、restore-paging 等长测试）与其它 crate 的跨 crate 回归——不在本切片定向命令内，按约定由合入后既有 CI 收尾。与 artifact.read 相关的既有 compose 断言只 `contains("continue with artifact.read reference={reference}")`，该前缀未变（行内续读是其后缀追加），未破坏；但也未在本机重跑验证该测试。
- 8 MiB 扫描预算之内的行内续读已闭合；超出扫描预算的工件仍只能读已扫前缀（footer 诚实声明 totals incomplete）——W06 已声明取舍，本次未改。病态输入（显式 `end_line` 接近 `usize::MAX` 且扫描未完）下游标饱和（`saturating_add`），不可由真实 footer 流到达，记为已知边界。
- F6 的 `partial scan` 计数口径：`scanned_files` 只计实际做了词法扫描的源文件（与 metadata 既有 `files_scanned` 同口径），非源语言文件被读取但不计入——与既有行为一致，未重定义。
- 未做：paid/供应商实验、Windows CI run、AST/向量检索（明确不做）。
- 审查方式：artifact.rs / code.rs / tools/mod.rs / broker.rs 完整读取；TOOL_INVENTORY.json、agent-conformance（builtin.rs、checks.rs 的 parity 部分）、fs.rs/search.rs footer 消费点、agent-compose restore-paging 断言为局部读取；上表命令均为本机（Windows）真实执行。
