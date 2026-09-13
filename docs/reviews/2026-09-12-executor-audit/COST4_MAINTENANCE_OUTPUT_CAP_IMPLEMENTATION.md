# COST-4 首半回执：请求级输出上限（D02）

- 日期：2026-09-12（工作树，基线 `685b6bbb` 加未提交树，未提交）
- 归属：M18 **C 线 COST-4** 首半；D02 指出的「metadata 的 `output_char_cap` 不进入 provider 参数」缺口。

## 修了什么

压缩器的输出上限此前只写在请求 metadata 里——provider 按主 profile 的 4096 上限生成，回复回来才裁到 512 字符。**截短从未等于生成成本已被限制**。现：

1. `ModelRequest` 增 `max_output_tokens: Option<u32>`（serde default＋skip 序列化；`ModelRequest` derive `Default` 以兼容既有构造点——`CancellationToken`/`Value` 均可实现 Default）。
2. provider 双协议在 `send_max_tokens` 协商开启时以请求 cap 覆盖 profile cap：Chat `max_tokens`、Responses `max_output_tokens`。端点未协商该字段（`send_max_tokens=false`）时**行为逐字节不变**——已知拒绝该字段的端点不会因压缩调用开始收到它。
3. `ModelBackedCompactor` 把 `COMPACTION_OUTPUT_CHARS`（512）作为请求级上限写入：生成在压缩边界处停止，回复后的字符截短退为兜底。

## 回归（新增 2 项）

- provider `request_level_output_cap_overrides_the_profile_cap`：双协议下请求 cap 覆盖 profile cap（512 vs 2048）；`send_max_tokens=false` 时 wire 不出现该字段；
- compose `the_output_cap_reaches_the_provider_request`：捕获型 transport 断言压缩请求携带 `Some(512)`。

## 实际检查

- `cargo test -p provider-openai --lib` **120**；`agent-compose` 12 个测试目标全绿；`agent-contracts` **173**；clippy（三 crate）0 警告；fmt 通过；`doc_consistency.py` OK。
- 既有 24 处 `ModelRequest` 字面量经 `..Default::default()` 机械适配（本轮曾出现批量脚本误伤括号结构，已逐处修复并以 fmt/clippy/全目标测试复验）。

## 后半（2026-09-12 同日补齐）：独立 maintenance transport

- `compose::try_maintenance_transport_from_env()`：`MAINTENANCE_TIMEOUT_SECS`（可选，≥1）设置时构建**独立压缩 transport**（同凭据/端点/重试预算，时间上界由操作者设定；demo 模式忽略——mock 不计费；设置了 override 但无凭据 fail-closed 报错）。
- `build_context_engine` 增第 4 参 `maintenance_model: Option<Arc<dyn ModelTransport>>`：**maintenance transport 拥有压缩器**，缺省回退主模型（历史单 transport 行为逐字节不变）。
- 三入口一致接线：宿主 main.rs（激活时打印横幅）、TUI session.rs/cli.rs。
- **端到端测试** `the_maintenance_transport_owns_the_compactor`：主模型计数器为零、折叠走向失败的 maintenance 模型，且该调用按 COST-1 语义入 Unknown 账——所有权由行为证明而非注释。

**COST-4 边界（如实记录）**：重试预算继承主 transport 的有界默认（3×500ms）——独立重试旋钮留待运营需要；模型选择按 D02 留待语义回归之后；引擎侧单维护 token 总预算 `max_compactor_tokens_per_maintain` 已存在于 `RollingConfig`（默认 MAX，按运营需要收紧属 A 线运营面）。

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；真实端点的生成成本对照属 COST-5，照旧 NOT_RUN。
- 真实端点的生成成本对照属 COST-5，照旧 NOT_RUN。
