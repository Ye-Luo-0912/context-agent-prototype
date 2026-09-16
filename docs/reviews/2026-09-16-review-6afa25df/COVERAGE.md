# 本轮实际阅读覆盖

固定基线 `6afa25dff0230fec982ee7d836aa7811ad901d0c`。

**本表不是全仓已通读证明。** 元数据清单、源码区间与全文阅读分别标记；输出截断不按完整阅读计算。

本轮涉及 17 个独立源码/文档文件；其中标记全文的只限返回到文件结尾的内容，`mod tests;` 指向的子文件不自动计入。

| 文件 | 请求区间 | 覆盖类型 | 说明 |
|---|---|---|---|
| `docs/NEXT_TASKS.md` | 1–80 | partial_response | 只读取当前队列与前部历史；输出截断。 |
| `crates/agent-runtime/src/execution/freshness.rs` | 1–330 | ranges | 归因、mutation footprint、观察与失败义务；不是全文件。 |
| `crates/agent-tui/src/session.rs` | 115–430, 430–700 | ranges | 命令 worker/提交/生命周期与主循环，未通读所有命令和测试。 |
| `crates/context-simple/src/materializer.rs` | 630–1060 | ranges | external 描述符及 foreground 冷快照 fallback。 |
| `crates/context-simple/src/engine.rs` | 2340–2535, 3220–3555 | ranges_plus_commit_diff | 冷解析/owner/材料化/ACK；existing card 比对来自父提交 diff，对比清单证明本次 docs head 未更改该逻辑。 |
| `crates/agent-runtime/src/actor/tools.rs` | 1010–1220, 1320–1650 | ranges | 模型结果接受与 ACK、工具结算/后续推进。 |
| `crates/context-simple/src/tests/batch_required_plan.rs` | 1–380 | partial_response | 主反例及预算反例完整可见；混合反例尾部输出截断。 |
| `clients/dotnet/Agent.Client/AgentConnection.cs` | 1–350, 351–720 | full_returned_source | 第二段已返回文件结尾。 |
| `clients/dotnet/Agent.Client/ResumableSession.cs` | 1–345, 346–700 | full_returned_source | 连接、pump、重同步、API与Dispose，已至文件尾。 |
| `crates/agent-contracts/src/model_cache.rs` | 1–370 | full_returned_source | 文件到末尾（外部 tests 子模块未通读）。 |
| `crates/context-simple/src/access.rs` | 1–340 | full_returned_source | 文件到末尾含内联两项测试。 |
| `crates/provider-openai/src/prompt_cache.rs` | 1–380 | full_returned_source | 模式配置全文；tests/endpoint_shape 子模块未在本轮通读。 |
| `clients/dotnet/Agent.Client.Tests/EventStreamTests.cs` | 400–720, 710–775 | ranges | 重同步与溢出回归；不是整个测试文件。 |
| `crates/provider-openai/src/lib.rs` | 1–150, 470–825, 800–1150, 1150–1270 | ranges | 主要流循环与wire mapper；未覆盖全部实现/内联测试。 |
| `crates/provider-openai/src/sse.rs` | 1–280, 280–365 | ranges | framer、wire usage、parser、accumulator 前段。 |
| `crates/agent-runtime/src/actor/model.rs` | 1370–1600 | ranges | 最终装箱之后的验证、surface与实际ACK构造。 |
| `clients/dotnet/Agent.Client/MetricsSession.cs` | 1–390, 385–430 | full_returned_source | 资源采样与父关系读取到末尾。 |

## 未在本轮完整通读的范围

其余 Runtime/Core/Context/工具/进程/存储/平台实现，provider 其他子模块，所有 workspace 测试和脚本，GUI，SDK 其余文件，以及大型设计文档；过去轮次的阅读不冒充本次全部复核。递归树输出本身存在展示截断，本表不据此计算全仓源码总行数。

搜索默认分支曾返回其他 ref，所有结论回到固定 SHA 文件核对。CI 记录仅用于确认该 SHA 的已知验证状态，不是新增反例已经运行的证据。

本环境没有 Cargo/Rustc/dotnet，未运行仓库测试。未获取到本地全仓克隆/归档。原始工具引用与范围见 READING_MANIFEST.json。
