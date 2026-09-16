# A1 落地回执：审批可核对与多行文本渲染（U1＋U2）

- 基线：`d92564bcfa41dda44f752e1e49e88a321abdf942`
- 提交：`22ab97a68838d55397efc6c815433cdb4c3e3ccc`（`tui: A1 informed approval & multiline rendering (U1+U2)`）
- 范围：`crates/agent-tui/src/ui.rs`、`state.rs`、`session.rs`
- 依据：[REVIEW.md](REVIEW.md) U1/U2、[NEXT_ACTIONS.md](NEXT_ACTIONS.md) A1

## 用户动作

待审批的长命令、长路径或大段替换内容，能在按 `[y]` 之前翻到尾部核对；代码块、错误栈、计划按原行结构显示，而不是被压成一行。

## U1（P1）：审批正文可完整查看

| 项 | 修复前 | 修复后 |
|---|---|---|
| 审批数据 | `args_preview: String`，总参数预览截取前 220 字符 | `detail: Vec<String>`，保留**完整**请求：工具名、risk、request_id、每个参数全文 |
| 审批面板 | 固定在 `Length(3)` 布局槽（扣边框只剩 1 行），无滚动 | 待审批时切到专用布局（history 让位，面板取 `Min(8)`）；`PgUp/PgDn` 翻页 |
| 截断提示 | 无 | 渲染器自身硬上限（1 MiB，Core 已先约束请求）触发时插入显式 `[!] arguments were truncated …` 行；对话区摘要改用 `…` 标记并指向面板 |
| 导航键 | 审批中只认 `y/n/Enter/Esc`，PageUp 被丢弃 | `classify_approval_key`（纯函数）分离「回答」与「导航」；`PgUp/PgDn` 调 `approval_scroll` |
| 确认身份 | 读取当时展示的 request_id 后即 `clear_approval` | 回答绑定当前 `request_id`；仅当屏上请求仍是刚回答的那个才清除，**过期确认不会批准随后到达的新请求** |

对话区二次摘要仍保持有界（`ARG_VALUE_CAP = 120` 字符/值、`ARG_PREVIEW_COUNT = 8` 项），但尾部截断现在会标 `…`，超出项数会注明条数并提示翻面板——不再冒充完整。

## U2：多行结构与布局口径统一

- `conversation_lines` 把每条正文按 `'\n'` 拆成多条真正的 `Line`（`Line::from(String)` 会丢换行，不是多行容器）；`render_context_panel` 同样由一条 `'\n'` 拼接 Line 改为两条 Line。
- 折行与滚动共用一条显示宽度规则：新增 `display_width` / `char_display_width` / `is_wide`，`conversation_scroll`、`wrapped_rows`、`approval_scroll_offset` 都由它推导，不再用「总宽度 ÷ 宽度」独立估算。
- 输入光标改用**终端显示列宽**（CJK/宽字符算 2 列），并在输入超出面板时加横向视窗（`scroll((0, hscroll))`），长输入不再把光标顶出可视区。

## 回归（红先，并用变异恢复法在本机复验）

新增 5 条测试，断言均经过**真实 `ui::render` ＋ Ratatui `TestBackend`** 的 buffer，而非内部字符串取子集：

| 测试 | 断言要点 |
|---|---|
| `ui::render_tests::approval_detail_is_full_and_scrollable_to_the_tail_sentinel` | 翻页后尾部 sentinel 出现在 buffer |
| `ui::render_tests::long_args_with_shared_prefix_are_verifiable_at_the_tail` | 前 120 字符相同的两条长参数，尾部关键目标可区分 |
| `ui::render_tests::multiline_body_keeps_each_source_line_on_its_own_row` | 多行正文的第二行落在自己的 buffer 行 |
| `ui::render_tests::chinese_input_cursor_lands_on_the_display_column` | 中文输入光标落在显示列 |
| `ui::render_tests::long_chinese_input_stays_within_the_input_viewport` | 长中文输入光标不越出输入区 |
| `state::status_projection_tests::approval_detail_keeps_full_arguments_and_marks_truncation` | 12 个长参数全文保留、截断被标记 |
| `session::approval_key_tests::classify_approval_key_maps_y_enter_to_allow_and_n_esc_to_deny` | 回答键与导航键分离 |
| `session::approval_binding_tests::expired_request_confirmation_does_not_approve_a_later_request` | 过期确认不批准新请求（真实 broker＋gate，无 PTY） |

**变异恢复法复验**（sha256 校验备份，改完立即还原并核对哈希）：

- 把 `conversation_lines` 还原成 `Line::from(message.content.clone())` → `multiline_body_keeps_each_source_line_on_its_own_row` **FAILED**。
- 把 `approval_scroll_offset` 钳死为 `0`（等价修复前「不能滚动」）→ `approval_detail_is_full_and_scrollable_to_the_tail_sentinel` **FAILED**。

两次还原后 `sha256sum -c` 均 OK。

## 已执行验证

```
cargo test -p agent-tui          → 75 passed; 0 failed（含 main.rs 内 guard/render/state/cli/session 全部）+ real_binary_startup 2/0
cargo clippy -p agent-tui --all-targets -- -D warnings → 0（首次运行有 4 处 clippy::io_other_error 与 1 处 collapsible_if，已修）
cargo fmt -p agent-tui -- --check → clean
```

## 限制（如实）

- **未做**真实 PTY 下的端到端交互验证；审批翻页与多行渲染的证明是 `TestBackend` 级。
- `display_width` 用的是内联宽字符表（覆盖 East-Asian Wide/Fullwidth 主流区间），未直接依赖 `unicode-width` 的完整 Unicode 表 —— 组合字符/罕见表宽可能与 ratatui 存在个别差异。测试只覆盖了中文与 ASCII 混合场景。
- 审批面板的 1 MiB 硬上限是防御性的；Core 侧请求上限未在本轮复核。
- 本轮**没有**改动批准/拒绝的 Core gate 路径，权限语义不变。
- 审批数据的完整保留意味着长请求在 UI 内存中的驻留量增加；仍受单条请求上限约束，但不是显式的独立预算。
