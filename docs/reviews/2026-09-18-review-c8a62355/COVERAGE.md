# 实际读取范围 — c8a62355

所有源码定位固定到 `c8a623554f762505061cdfdd7466ccaa995a3de0`。

**20 个不同文件，包含文档和部分区间；不是全部仓库逐行审查。**下表范围是请求／核对范围，宽返回被截断时明确说明；不将未显示段落视为已读。搜索命中、目录盘点和旧版本审查不自动提升为本版本正文覆盖。

| 路径 | 本轮范围 | 说明 |
|---|---|---|
| `docs/CURRENT.md` | 见说明 | 全文请求返回被截断；只使用实际返回内容 |
| `crates/agent-runtime/src/actor/lifecycle.rs` | 300–650 | 指定区间 |
| `crates/tool-runtime/src/tools/artifact.rs` | 1–420, 380–500, 700–815, 890–1054, 1050–1500, 155–380 | 部分宽区间返回截断；155–420 核心路径补读；未覆盖完整测试文件 |
| `crates/agent-runtime/src/actor/tools.rs` | 1–310, 1810–2225, 2410–2675 | 指定区间 |
| `crates/agent-runtime/src/actor/model.rs` | 950–1160, 1920–2105 | 指定区间 |
| `crates/agent-process/src/host.rs` | 460–750, 760–1170 | 指定区间；较宽返回可能截断；未通读全文件 |
| `docs/reviews/2026-09-16-review-71f8a586/C0_PROOF_SUPERVISION_RECEIPT.md` | 见说明 | 全文 |
| `crates/agent-workspace/src/broker.rs` | 1–139, 140–280 | 生产段与部分测试入口；不声称全部测试已读 |
| `crates/agent-contracts/src/model.rs` | 380–700 | 指定区间 |
| `crates/agent-compose/tests/kv_production_sequence.rs` | 1–290, 290–600, 600–860, 860–1100 | 分段读至 EOF，全文 |
| `crates/agent-storage/src/lib.rs` | 1–360, 440–810, 810–1165, 1–190, 180–360 | 指定区间；open 与 compact 关键段另以窄范围核实 |
| `crates/agent-workspace/src/journal.rs` | 1–240 | 指定区间 |
| `crates/agent-runtime/src/task.rs` | 1070–1380, 1450–1575, 1570–1880 | 全文请求另有截断；以下范围补读，不声称全文 |
| `crates/agent-tui/src/cli.rs` | 310–650 | 指定区间，关注失败终态 |
| `clients/dotnet/Agent.Client/Envelope.cs` | 1–300 | 指定区间 |
| `clients/dotnet/Agent.Client/ProtocolDto.cs` | 1–300 | 指定区间返回至文件尾 |
| `crates/agent-process/src/integrity.rs` | 1–350 | 返回至 EOF，全文 |
| `crates/agent-runtime/src/status.rs` | 1–330 | 生产段与部分测试入口 |
| `crates/context-simple/src/store.rs` | 1360–1650, 1640–1855, 1850–2140 | 指定连续范围1360–2140，含 reconcile |
| `crates/context-simple/src/engine.rs` | 1960–2280, 2850–3210, 3210–3555 | 指定区间，包含 reconcile/materialize/ACK 与部分生命周期 |

## 结构与验证资料

- 核对根 tree、crates tree 的 20 个 workspace 目录、上轮到本 SHA 的提交差异、SDK 目录、当前 CI。目录覆盖不代表对应 crate 全部代码读完。
- 根树：/response/turn591；crates 完整树：/response/turn599；对比：/response/turn571；SDK 目录：/response/turn608。
- 当前 main：/response/turn629；CI 最后状态：/response/turn630；jobs 使用 GitHub.fetch_workflow_run_jobs 的真实返回。
- 其他未列文件、已列文件未读范围、运行时不可见的在途工作树均不作“无问题”结论。

## 本轮执行矩阵

| 项目 | 结果 |
|---|---|
| 固定 SHA GitHub 源码读取 | 已执行 |
| 全仓本地 clone / 编译 | 未成功 / NOT_RUN |
| Rust / .NET 回归 | NOT_RUN；环境没有相应工具链 |
| Windows / TUI 真实终端实验 | NOT_RUN |
| Python 冷目录 / 工件机制移植 | 已执行，输出 MECHANISM_RESULTS.json |
| Linux 独立 open 句柄上的 flock / truncate 临时文件实验 | 已执行；不等于 Rust journal 端到端 |
| 付费供应商 / 缓存净金额实验 | NOT_RUN |
| 仓库修改或推送 | 未执行 |

外部事实只使用 Rust、Microsoft、OpenAI 官方文档。所有建议回归均是待实施规格，不以机制探针替代。
