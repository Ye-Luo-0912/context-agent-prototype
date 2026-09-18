# 实际读取范围 — bcacf41b

分支 `codex/runtime-endurance-full-plan`，固定SHA `bcacf41b9104db6ebada7adfc2a95de5e341f49b`。

实际读取 **22个不同文件**。全文、连续区间、截断返回严格区分；重叠区间不重复算文件。38项变更清单只是diff盘点，不是38份源码完整通读。

| 文件 | 本轮范围 | 方式 | 来源 |
|---|---|---|---|
| `docs/CURRENT.md` | 全文请求（截断） | 部分：全文请求被截断；仅审阅返回部分 | turn681file0 L2 |
| `docs/NEXT_TASKS.md` | 1–90 请求（截断） | 部分：指定区间返回被截断 | turn682file0 L2 |
| `docs/experiments/runtime-endurance-v1/RUN_2026-09-19-FULL-PLAN.md` | 全文 | 全文 | turn684file0 L2 |
| `docs/experiments/runtime-endurance-v1/PLAN.md` | 全文请求（截断） | 部分：全文请求被截断 | turn685file0 L2 |
| `scripts/runtime_endurance_full_campaign.py` | 全文 | 全文 | turn686file0 L2 |
| `scripts/runtime_endurance_incremental_runner.py` | 全文 | 全文 | turn687file0 L2 |
| `docs/experiments/runtime-endurance-v1/RUN_2026-09-19-L1.md` | 全文 | 全文 | turn688file0 L2 |
| `docs/experiments/runtime-endurance-v1/RUN_2026-09-19-L1-COMPLETE.md` | 全文 | 全文 | turn689file0 L2 |
| `crates/agent-runtime/src/actor/lifecycle.rs` | 1–185；560–825 | 区间 | turn690file0 L2, turn693file0 L2 |
| `crates/agent-runtime/src/actor/maintenance.rs` | 920–1165 | 区间 | turn691file0 L2 |
| `crates/agent-runtime/src/actor/model.rs` | 735–940 | 区间 | turn692file0 L2 |
| `crates/agent-runtime/src/execution/state.rs` | 1–320；630–825；980–1305；1320–1660；1800–2160 | 区间 | turn694file0 L2, turn699file0 L2, turn700file0 L2, turn706file0 L2, turn707file0 L2 |
| `crates/agent-runtime/src/prompt.rs` | 1–235；300–630 | 区间 | turn695file0 L2, turn696file0 L2 |
| `scripts/runtime_endurance_preflight.py` | 全文 | 全文 | turn698file0 L2 |
| `crates/agent-runtime/tests/turn/failure_resume.rs` | 1–275；275–545；545–820（文件实际结束于794） | 全文（连续区间覆盖） | turn701file0 L2, turn703file0 L2, turn708file0 L2 |
| `crates/agent-runtime/src/actor/safepoint.rs` | 280–635；630–800 | 区间 | turn702file0 L2, turn709file0 L2 |
| `crates/context-simple/src/gc/reachability.rs` | 180–285；635–680；670–965 | 区间 | turn705file0 L2, turn714file0 L2, turn718file0 L2 |
| `docs/experiments/runtime-endurance-v1/scenario.json` | 全文 | 全文 | turn710file0 L2 |
| `docs/walkthroughs/2026-09-18-t8-kv-live.md` | 全文 | 全文 | turn712file0 L2 |
| `crates/agent-runtime/src/surface.rs` | 130–310 | 区间 | turn713file0 L2 |
| `.github/workflows/ci.yml` | 1–90 | 区间 | turn715file0 L2 |
| `crates/agent-tui/src/cli.rs` | 1–290 | 区间 | turn717file0 L2 |

## 元数据与失败读取

- 分支列表、固定commit、父子比较、精确SHA Actions、TUI目录均已查询。
- 新分支最后仍为bcacf41b；main仍24c354cb。
- 原始full-campaign-receipt同路径404；headless.rs猜测路径404后读取真实cli.rs。404不记入已读文件数。
- /response/turn680的大commit diff只检查部分文本与文件名，不视作所有patch阅读。
- 默认分支code search仅用来定位，正文结论使用固定新SHA。

## 本轮未完成覆盖的区域

未全读Core效果提交/WAL/Workspace路径隔离、Provider全部协议/重试、Context所有GC/搜索文件、各工具全部实现、SDK/.NET、TUI完整session/state/ui、全部测试及应用成品。旧会话曾读过的代码不是本轮新SHA通读证明，不能补进本轮覆盖率。

## 环境和执行

本环境Python/Git/Node可用，Cargo/Rustc/.NET未找到。Git远端读取遇DNS失败；代码通过GitHub连接器读，不存在可用于cargo test的本地checkout。
七项机制检查实际运行，类型与前提写入MECHANISM_RESULTS；没有真实供应商请求，没有改变仓库。

所有源路径、引用和38项变更清单见SOURCE_MAP.json。
