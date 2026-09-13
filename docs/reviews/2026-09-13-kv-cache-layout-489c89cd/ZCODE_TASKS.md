# ZCode 实施清单（KV 布局 + 工具表面）

基线：`489c89cd`（F1–F5 已在远程 main）。  
权威说明：[REPORT.md](./REPORT.md)、[VERIFICATION.md](./VERIFICATION.md)。  
**不要用 Cursor 云端额度做这些切片**（本清单留给 ZCode 免费额度）。

> **实施状态（2026-09-13）：A0、A1/R7、C1/R2、C2/R3、C3/R1+R4、C4/R6 已全部落地合入 main**
> （`aaeb827f`、`3bd326a2`、`3c9f600e`、`8dca1b63`；CI run `34765619847` 全绿）。
> 回执：A0 → [A0_PROOF_SUPERVISION_ROOT_CAUSE.md](./A0_PROOF_SUPERVISION_ROOT_CAUSE.md)；
> A1 → [A1_R7_GREP_SCHEMA_RECEIPT.md](./A1_R7_GREP_SCHEMA_RECEIPT.md)；
> C1–C4 → [C_LINE_PROVIDER_RECEIPT.md](./C_LINE_PROVIDER_RECEIPT.md) 与 NEXT_TASKS.md 状态表。
> 仅 **C5/R5**（条件性）未开始。下方清单保留为原始停止条件，供复核。

## 硬约束

- 勿重开 F1/F3；F4 续跑实现已在，只补 schema（R7）。
- 不新建第二套任务表 / 平行遥测 / 无限 append-only 历史。
- 缓存不得延长失效授权、把旧文件冒充当前版本、或把工具输出抬成 system。
- 契约改动（`ModelInput` / `ModelRequest` / `PromptReuseBoundary`）尽量单一维护者改；其它线只填字段。

## 推荐顺序

### A0 — 调查 `proof_supervision` CI 失败

- **目标：** 查清宿主死亡后进程树未在期限内退出的原因（生产监督 / 夹具 / 环境）。
- **停止：** 有确定结论与回归；保留确切进程身份事实。禁止只加大 timeout 或删测。
- **检查：** `cargo test -p agent-compose --test proof_supervision`

### A1 / R7 — `search.grep` 模型可见 schema（先修）

- **目标：** 在 `SearchGrepTool::spec` 的 `input_schema.properties` 声明可选 `scan_continuation`（格式/长度/同查询语义）；经 `compact_for_model_surface` 与 provider wire 后仍在。
- **停止：** 用正式 ToolSpec 构建模型请求 → 按 schema 生成的续跑参数走 **dispatcher** 到第二批（禁止只测直接 `execute`）。`cursor` 仍仅结果分页。
- **检查：** `cargo test -p tool-runtime search`；conformance / surface digest 按仓库既有门禁。

### C1 / R2 — 稳定 `prompt_cache_key`

- **目标：** 仅在**已确认支持**的 endpoint profile 发送稳定、非明文敏感的路由键（隔离域/工作区、端点、任务、Main vs Maintenance）。同任务连续请求稳定；禁止每轮 UUID / `model_round` / 完整动态请求 hash。
- **停止：** 实际 HTTP payload 可见正确键；跨隔离域不混用；未知端点不发送专属字段。
- **检查：** `cargo test -p provider-openai prompt_cache`

### C2 / R3 — 显式写策略与空边界

- **目标：** 分开「端点能力 / 调用期望写策略 / 本次有无合法边界」。explicit-only 已确认时，无边界 → 该端点支持的不写缓存形状并记原因。修 compactor：不要静默走 ProviderDefault 写缓存。
- **停止：** 主调用、压缩、空/过期 hint、旧协议均有最终 wire 测试。
- **检查：** `cargo test -p provider-openai prompt_cache`；compactor 相关单测。

### C3 / R1 + R4 — 稳定证据基座边界

- **目标：** 勿把整个 `context_frame` 当一块稳定缓存。至少 B0（稳定 policy/facts）与 B1（阶段内仍有效证据）；易变 foreground / required_miss / 恢复投影 / attention 语义放到边界之后或动态段。状态字段不要破坏仍合法证据的前缀字节。
- **停止：** 只改焦点/进度时稳定基座字节与边界不变；新检索不插入 B0/B1 之前；真版本/权限变化立即失效。
- **检查：** `cargo test -p agent-runtime prompt`；`cargo test -p agent-contracts model_cache`；`cargo test -p agent-compose --test kv_cache_walk`（不加 `--ignored`）

### C4 / R6 — 每次真实 attempt 的用量账目

- **目标：** 失败已报 usage → 成功/放弃/取消时，已知数值不丢；同 attempt 多条 SSE 累计只结算一次；不同 attempt 可相加；未报告 ≠ 0。
- **停止：** failure-with-usage→success、两次 failure→give-up、usage 后取消、sink failure 等用例账目恰一次。
- **检查：** `cargo test -p provider-openai retry`

### C5 / R5 — 协议尾复用（有条件后续）

- **仅在 C1–C3 完成后评估。** 保持 call/result 配对与真实角色；不得为打点把 `function_call_output` 改成普通 user 文本。

## 建议验证命令（ZCode 环境）

```sh
cargo test -p agent-contracts model_cache
cargo test -p provider-openai prompt_cache
cargo test -p provider-openai retry
cargo test -p agent-runtime prompt
cargo test -p tool-runtime search
cargo test -p agent-compose --test kv_cache_walk
cargo test -p agent-compose --test proof_supervision
cargo fmt --all -- --check
```

真实供应商对照实验必须单独预算，忽略的付费测试不算已验收。
