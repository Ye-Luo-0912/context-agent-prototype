# 文档入口迁移应用回执（2026-09-14）

应用 [文档入口审查](REVIEW.md)（主基线 `2b43186b`，收尾核对 `27828014`）与 [迁移任务](MIGRATION_TASK.md)。本回执记录实际执行内容与验证结果。

## 实际执行

1. **CI 阻塞修复**（审查定位的 `27828014` CI 失败根因）：`crates/tool-runtime/src/tools/session.rs` 的 `assert_eq!(output.metadata["signal"].is_null(), false)` 改为 `assert!(!output.metadata["signal"].is_null())`（A1 信号测试引入的 clippy 布尔断言写法）。本地 `cargo clippy -p tool-runtime --all-targets` 0 警告。
2. **入口替换（替换而非追加）**：AGENTS.md（3.2 KiB，稳定约定＋阅读入口，移除 M17/阶段/工单）、docs/CURRENT.md（3.7 KiB，当前事实与范围）、docs/NEXT_TASKS.md（5.3 KiB，仅存有动作任务）、docs/ROADMAP.md（2.5 KiB，能力阶段）、docs/STATUS.md（0.3 KiB，纯导航）按草案全文替换。五份合计约 15 KiB（替换前 CURRENT＋NEXT 约 304 KiB＋AGENTS）。
3. **历史保全**：旧五入口＋state.json 全文快照存入 `docs/archive/entries-2026-09-13-2b43186b/`（含 state-PROVENANCE.md 说明 M15 closing_window 与 LT-EVAL-06 证据位置）；git 提交 `2b43186b` 是权威保全基线。冻结实验与全部 reviews/ 目录原位、原字节未动；reviews/ 内不放 AGENTS.md。
4. **README 定点替换**：仅替换"The concrete product target…"至 Architecture 前的旧 M15–M17/N0/CI 段落为后端主体描述；`context-baselines` crate 说明区分 baseline 形态（无 compactor 的 fixed marker）与产品组合注入的真实压缩器。
5. **state.json 职责收缩**：活跃文件改为 schema v2 导航/来源元数据（不再是当前状态权威）；v1 的 head/ci/milestone/m15 字段留在归档快照；M15 证据位置（M15_ACCEPTANCE.md、lt-eval-06 evidence）在 provenance 与 state.json 中点名保留。
6. **doc_consistency.py 同步**：必查字段改 v2 导航 schema；m15 closing-window 报告存在性检查转向归档快照（不删检查）；新增入口角色结构检查（五入口存在/非空/互相引用/归档快照齐全）；成功文案收窄为「structure/links/toolchain checks passed」，不再声称「state agree」。
7. **CONTEXT_LIFECYCLE 补完整性条款**：新增「不可逆删除的完整性条件」节——恢复根完整 ≠ 元数据完整；pending owner 非 ownerless；依赖 stored 出边的删除规划在 pending 未清时延期（仅把 pending id 加入根集合不保护未读出的边）；临时 I/O 失败保持可重试来源。与 NEXT_TASKS 的 B2 调用方任务同源，实现归该任务。

## 验证

- `python scripts/doc_consistency.py`：OK（13 live docs；structure/links/toolchain checks passed）。
- `cargo clippy -p tool-runtime --all-targets`：0 警告（CI 阻塞项修复）。
- `cargo test -p tool-runtime --lib`：277/277。

## 如实记录的限制

- 未重写 ARCHITECTURE.md（151 KiB）与 AUDIT_TODO.md（84 KiB）——按迁移任务约定，它们随对应代码切片逐步整理，本轮只修入口与状态职责。
- B2 调用方完整性传播（hydration → GC/reconcile）本轮只落契约条款与任务卡，代码与故障注入回归归 NEXT_TASKS 的 B2 任务。
- 新提交 `27828014` 的 CI 修复（本回执第 1 条）需推送后由远端 CI 确认；本回执只记录本地 clippy 修复。
- 归档目录中的旧入口 Markdown 保留原相对链接字面（指向同目录不存在文件）——git 提交 `2b43186b` 是它们的解析基线，归档件以快照证据身份存在，不作为活跃导航（doc gate 只检查 live 文档链接）。
