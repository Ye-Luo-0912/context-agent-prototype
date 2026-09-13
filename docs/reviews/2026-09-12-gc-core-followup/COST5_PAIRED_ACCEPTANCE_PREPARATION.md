# COST-5 准备回执：配对验收脚本与记账提取器（真实执行 NOT_RUN）

日期：2026-09-13。COST-6/7/8 代码落地后的共同验收准备；本轮无任何付费调用。

## 交付物

- [`cost5_paired_run.py`](cost5_paired_run.py)：两种模式。
  - `--from-evidence DIR`：对既有运行证据（含 2026-09-11 flash-workflow 产物）重算 COST-5 记账表——双 lane（main/maintenance，按 `role`）、身份四分类（observed/estimated/unknown/**unreported**）、四个缓存桶（read/write/miss 各自独立、缺席即缺席，绝不折算零）、attempts/retries、压缩行身份与重试、GC 工作量（evicted/externalized/store bytes/io failures）、`bill_lower_bound` 完整性标志、wall_ms p50。所有管道字节经凭据清洗。
  - `--paired --out DIR [--rounds N] [--timeout S]`：真实双臂执行——**同一固定三任务**（a 跨文件重构＋pytest 验收、b 九域审阅验证、c 长输出＋取消/恢复，种子/授权/验收完全复用 2026-09-11 harness 并以 importlib 单源引用），A 臂=compose 默认预算，B 臂=COST-8 新产品旋钮（`MAINTENANCE_MAX_CALLS_PER_MAINTAIN=2`/`MAINTENANCE_MAX_TOKENS_PER_MAINTAIN=20000`/`MAINTENANCE_COMPACT_FAILURE_BACKOFF=4`），产物验收逐任务落 `*-verify.json`，输出配对表 `cost5_paired.json`。脚本自身只产出 Token 级记账；**降本结论保留给人**（同质量＋分桶不重叠＋恢复/资源不退化的完整门）。
- [`cost5_extracted_2026_09_11_baseline.json`](cost5_extracted_2026_09_11_baseline.json)：提取器对 09-11 真实证据的干跑产物＋交叉验证标注。

## 提取器交叉验证（对既有真实证据，零成本）

对 2026-09-11 flash-workflow 五段证据干跑，决策 lane 原始计数总额 **逐 token 等于**当时独立记录的回执总数：input **105,838** / output **3,907** / cached **72,704**。旧事件早于 CORE-4/COST-2，身份如实记为 unreported、计数保留为 raw 下界（不冒充 observed、不丢弃）——这正是 COST-6/7 语义在真实历史数据上的演示。

## 真实执行的阻塞与入口（如实记录）

1. **共享树相干性**：`agent-tui`/`agent-replay` 因并行 B 线在飞的事件新字段（`artifacts`/`final_output_digest`）消费点未收口而无法编译——真实运行的二进制（`target/debug/agent-tui.exe`）需待其窗口收口后重建。
2. **付费窗口决策**：双臂约 7 段 × 每段 ≤ `--rounds`（默认 12）轮真实调用；端点/模型沿用仓库固定 `eval.env`（DeepSeek 官方 deepseek-flash，与既有已验收实测同一端点）。执行命令：`source eval.env && python docs/reviews/2026-09-12-gc-core-followup/cost5_paired_run.py --paired --out docs/reviews/2026-09-12-gc-core-followup/cost5-run-<date> --rounds 12 --timeout 420`。

**COST-5 状态：PREPARED / NOT_RUN（真实调用）。** 降本声明门完整保留：同质量（三任务产物验收双臂通过）、计费分桶不重叠、取消/恢复/资源不退化、未知不当零；仅 Token 改善只声明 Token 改善。
