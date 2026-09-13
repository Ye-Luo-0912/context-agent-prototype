# COST-2 首片回执：供应商缓存字段映射与压缩调用计费透传（E05.4）

- 日期：2026-09-12（工作树，基线 `685b6bbb` 加未提交树，未提交）
- 归属：M18 **C 线 COST-2**（按供应商能力映射 cache-read/write/miss 和计费字段）；修复 E05 断点 4 的三处丢失。共享契约增量（ModelUsage/CompactionOutput/ContextCompaction/事件）按派发文档属 B 合入——相关文件当时无人在途（provider 侧多日未动），按「重叠切片一次执行、双边关闭」垂直合入。

## 修了什么（E05.4）

1. **DeepSeek 顶层 hit/miss 映射（Chat sse）**：`WireUsage` 增 `prompt_cache_hit_tokens`/`prompt_cache_miss_tokens`。缓存读优先取 OpenAI `prompt_tokens_details.cached_tokens`，缺位回退 DeepSeek hit；miss 保留为 provider 自己的 cache-write 报告。provider 未发送的计数保持 `None`（绝不发明零）；两种拼写并存时 details 优先。
2. **ModelUsage 增 `cache_write_input_tokens: Option<u64>`**（serde default＋`skip_serializing_if` ——未报告即不出现，既有金样字节稳定）。Responses 端 OpenAI 标准响应不含 cache-write 计数，保持 None（input−cached 可推导），留待真实现测的网关报告后再接线。
3. **压缩输出不再丢调用事实**：`CompactionOutput`/`ContextCompaction`/`RuntimeEvent::ContextCompacted` 增 `cached_input_tokens`/`attempts`/`retries`（serde default＝旧行 0＝未知，不是「一次」）；`ModelBackedCompactor` 把主 transport 的缓存计数与尝试记账透传进输出；rolling 报告→事件→GUI 全链贯通。
4. **GUI**：压缩成本行按事件原样呈现缓存读与尝试数（非零时），仍不并入主调用实测合计。

## 兼容性

- 三处新字段全部 serde default：旧事件/旧报告解码不变；`cache_write_input_tokens` 序列化跳过 None，`model_used_event.json` 等既有金样 roundtrip 字节稳定（回归中发现首版序列化漂移并当轮修复）。
- 消费端（eval metrics 分类、GUI 压缩行）在 COST-1 已按身份就绪，本片字段到达即自动可见；metrics 暂未加压缩缓存合计字段（按需后补，不触碰冻结产物白名单）。

## 回归（新增 4 项）

- provider `deepseek_top_level_cache_hit_and_miss_are_mapped`（hit=缓存读、miss=cache-write 保留）＋`details_cache_tokens_take_precedence_over_deepseek_hit`（拼写并存时 details 优先、无 miss 不发明零）；
- baselines `zero_usage_compaction_is_still_accounted_with_its_identity` 扩展：cached/attempts/retries 经「引擎→报告→事件」全链存续断言；
- protocol fixture `event_context_compacted.json` 扩展三字段，Rust 侧断言逐字段值（.NET 侧读同一金样原始字段，117/117 含既有断言）。

## 实际检查

- `cargo test -p provider-openai --lib` **119**（117＋2）；`agent-contracts` **173**；`agent-eval` **223**；`context-baselines` **18**；`agent-platform-protocol` **47＋13**；`agent-compose` 12 个测试目标全绿；`agent-runtime` actor **86**、turn effects **16**。
- `cargo clippy`（六 crate）0 警告；`cargo fmt` 通过；`dotnet test` **117/117**；桌面 build 0 警告；`doc_consistency.py` OK。

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；provider wire 改动仅离线 fixture 单测覆盖，真实 DeepSeek/OpenAI 端点的字段观测照旧 NOT_RUN（属 COST-5 阶段验收）。
- `context-simple` dynamic 引擎的 `pending_compactions` 仍保留「token 非零才入账」门槛（E05.2 同型缺陷的 dynamic 版）——`engine.rs` 属 A 线，本片只完成编译适配未改其语义，已留接口请求。
- `agent-tui` 对并行会话新加的 `RestoreEvidenceDegraded` 变体的 match 缺口在回执时点仍未收口（非本片改动面）。
- 费率/账单分桶与真实成本对照归 COST-5；不做本地前缀长度省钱声明。
