# 核实回执：供应商 KV 缓存与请求布局续审

日期：2026-09-13  
审查基线：`489c89cd7d139f4e721348ec9c54054700d93b7b`  
核实方：Grok Bot（对照 GitHub raw 源码；未在本机跑 cargo / 未付费模型）

## 旧项

| 项 | 结论 |
|---|---|
| F1 spill 严格解析 / 四类 owner 查重 | **已修好，勿重开**（`checkpoint.rs`：`spilled_entries_from_value`、`reject_duplicate_spill_ownership`） |
| F3 按窗口需求 | **已修好，勿重开**（`prompt.rs`：`FileBodyWindow` / `row_satisfies_demand`） |
| F4 扫描续跑实现 | **后端已有**；模型 schema 仍缺，见 R7 |
| F2 / F5 | 以 main 回执为准；保留规模边界，不宣称无限历史 |

## 本轮发现

| ID | 结论 | 证据摘要 |
|---|---|---|
| **R1** | **成立** | `ModelInput::into_request`：`CurrentStateLast` 的 `prefix_len = system_policy.len() + context_frame.len()` |
| **R2** | **成立** | Responses wire 有 `prompt_cache_breakpoint` / `prompt_cache_options`；全仓 `provider-openai` **无** `prompt_cache_key` |
| **R3** | **成立** | 仅 `boundary_index.is_some()` 时写 `prompt_cache_options`；`compactor.rs` 构造的 `ModelRequest` 无复用边界 |
| **R4** | **成立** | `render_selected_item` 含 `workspace_identity=current` / `attention` / `semantic`；`omit_selected_file_body` 可跨层省略正文 |
| **R5** | **成立（后续片）** | 边界只覆盖 System/User 文本前缀；协议尾复用另立切片 |
| **R6** | **成立** | `retry.rs`：`carried_usage` 为最近一次失败 usage；成功分支只 `stamp_attempt_usage` 成功 output；等待取消可直接 `Cancelled` |
| **R7** | **成立（应先修）** | `GrepArgs`/`execute` 支持 `scan_continuation`，但 `SearchGrepTool::spec` 的 `properties` 仅 `pattern`/`path`/`limit` |

## CI 注记

基线 SHA 曾出现 `proof_supervision`：`timed out: exact proof tree exit`。根因未在本回执定位；**不要只加 timeout 或删测**。实现侧由 ZCode 按 `ZCODE_TASKS.md` A0 处理。

## 明确未做

未改产品代码；未测真实降本；未声称新布局已落地。实施清单见同目录 `ZCODE_TASKS.md`。
