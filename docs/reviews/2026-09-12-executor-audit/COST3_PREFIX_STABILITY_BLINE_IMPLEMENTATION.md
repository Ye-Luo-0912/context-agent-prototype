# COST-3 实施回执（B 线部分）：组装开销与热路径诊断清理（D04/D05）

基线：`685b6bbb` ＋ 既有未提交工作树。对应 [TASKS.md](TASKS.md) COST-3 与 [REPORT.md](REPORT.md) D03/D04/D05。工作树落地（未提交、未推送、未跑远端 CI）。**按共享契约分工的归属说明**：D03 的 prompt 证据渲染归 A 线（`prompt.rs` 正由 A 线会话合入其半）；本回执覆盖 B 线所有物 `actor/model.rs` 的 **D04/D05**。

## 用户能获得什么

预算裁剪循环不再为每次比较重新序列化整个消息列表——组装开销从「随丢项次数线性放大的全量克隆」降为「每次重组后一次求导」；热路径上的无条件 stderr 调试输出删除（工具面事实本就在 `ToolSurfacePlanned` 持久事件里）。语义与权限完全不变：唯一真值仍是最新组装的 `input`，最终拒绝与其保守余量保持，不通过扩大预算掩盖失败。

## 实现（`crates/agent-runtime/src/actor/model.rs`）

- **D04**：items 裁剪循环、foreground 裁剪循环、surface-omit 循环三处的 `while packed_total(...)` 条件——原来每次比较都经 `into_messages()` 全量克隆序列化——改为**跟踪值**：`input_total`/`packing_total`/`packed_now` 在每次 `assemble_model_input` 后求导一次、循环条件只读变量。末尾的 `estimated_input_tokens`/`packing_input_tokens` 直接取跟踪值（删除最后一次多余的重复求导）。`approx_tokens` 保持引擎侧启发式身份不变；装配行为逐字节不变（循环体只改求导时机，不改裁剪顺序与判定）。
- **D05**：删除无条件 `TEMP-DBG` stderr 块（每轮打印全部 tool schema 名）。

## D03（prompt 半）状态：A 线合入中，红测试已钉目标

本会话在 `prompt.rs` 落了一个**红-first 契约测试** `identical_items_assemble_identically_regardless_of_diagnostics_counts`——相同选中条目、不同引擎诊断计数的两轮，必须组装出逐字节相同的请求；当前 A 线合入中的后缀版 census 仍使其失败（红＝目标钉住）。census 事实的持久承载已存在：`ContextPrepared` 事件带完整 diagnostics，提示词内不再需要重复。该文件正由 A 线会话活跃编辑，本会话已按分工**完全退出**，不再写入。

## 实际执行的检查（本机 Windows，2026-09-12）

- `cargo test -p agent-runtime --test actor`：**86/86**。
- `cargo test -p agent-compose --test kv_cache_walk`：**5/5 通过**（前缀/复用边界探针不受影响）。
- `cargo clippy -p agent-runtime --all-targets`：**0 警告**。
- `prompt::` 测试的 census 失败属 A 线 COST-3 prompt 半在飞合并（本会话的红测试按设计处于红状态直到其合入满足「诊断不进请求」）。

## 边界与如实记录

- D04 是组装开销（CPU/分配）的确定性削减，**不是**缓存收益声明；稳定字节只是诊断，收益归 COST-5 实测。
- D03 的 prompt 半归 A 线：本会话的红测试是其验收标准之一。
- 未提交、未推送、未跑远端 CI；真实 provider 照旧 NOT_RUN。
