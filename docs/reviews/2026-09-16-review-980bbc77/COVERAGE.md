# 本轮实际覆盖 — 980bbc77

固定 `980bbc77f4086ebf8848f5c9afa16ce22fafe39f`。下表仅列本轮实际阅读的源文件/文档正文，不把历史聊天、文件名列表或搜索片段算成全文。共 23 个文件；其中 8 个按全文读取，其余仅列出的范围。

| 文件 | 实际读取范围（源文件行号） | 关注点 |
|---|---|---|
| `docs/reviews/2026-09-16-review-6afa25df/QA_QB_QC_QD_RECEIPT.md` | 全文 | 核对上轮回执；回执测试数不等于本轮执行 |
| `crates/agent-workspace/src/broker.rs` | 1–340 | 正文截断、元数据保留、工件与预算 |
| `crates/tool-runtime/src/tools/fs.rs` | 1–585 | 目录读取与 fs.read 原始范围声明 |
| `crates/agent-runtime/src/prompt.rs` | 300–530；550–1000；1320–1500 | 最终层次、恢复窗口、历史正文省略 |
| `crates/agent-runtime/src/output.rs` | 全文 | Runtime 兜底截断及内联测试 |
| `crates/agent-core/src/authority.rs` | 1–330 | 输出经纪接线、事件发布 |
| `clients/dotnet/Agent.Client/ResumableSession.cs` | 115–385 | 更新后的连接/队列代际安装 |
| `crates/agent-storage/src/lib.rs` | 1–370 | 操作日志初始化、持久状态边界 |
| `crates/context-simple/src/index/external.rs` | 1–650 | 冷热迁移、卡片认领、访问戳 |
| `crates/agent-workspace/src/handles.rs` | 全文 | 受限句柄及读取/准备写入 |
| `crates/agent-runtime/src/capability/mod.rs` | 1–300 | 能力登记、锁/状态职责与表面缓存 |
| `crates/agent-workspace/src/runtime_facts.rs` | 全文 | 运行事实与 Unix FIFO 测试 |
| `crates/agent-contracts/src/jcs.rs` | 全文 | 规范序列化、数值域与现有样例 |
| `crates/agent-contracts/src/operation.rs` | 1–220 | ArgumentDigest 构造 |
| `crates/agent-contracts/src/tool.rs` | 1–200 | 输出契约、上限与效果意图 |
| `crates/agent-contracts/src/schema_profile.rs` | 1–680 | 数值校验、数值上下限及模式编译 |
| `crates/agent-core/src/kernel/mod.rs` | 570–1335；1600–1890 | 检索输出、operation 参数绑定、派发与恢复辅助 |
| `crates/context-simple/src/engine.rs` | 1120–1440；1490–1705；1830–2085；2330–2660；3430–3780 | 冷页消费确认、查询续查、存储捕获与 GC 周边 |
| `.github/workflows/ci.yml` | 全文 | 构建/运行区分、Linux 分片选择 |
| `Cargo.toml` | 全文 | 20 个 workspace member、依赖声明 |
| `crates/agent-host/Cargo.toml` | 全文 | 宿主包身份及平台依赖 |
| `crates/agent-tui/src/session.rs` | 450–910 | 命令注册、正常/异常共同收尾 |
| `crates/provider-openai/src/lib.rs` | 425–780 | attempt 结算 helper、Chat 全部收口及 Responses 入口 |

## 结构盘点与限制

核对 root、20 个 crate 列表、agent-workspace 与 agent-contracts 的递归目录；目录列举只证明文件存在，不证明正文已读。搜索结果有时落在前一提交，只有重新按固定 SHA fetch 的正文才用于本轮实现结论。

没有完整通读：所有其余 crate 的当前版本源码、全部集成测试、SDK 剩余文件、GUI 当前版本、全部脚本与文档。对未读部分不作通过结论，也不能给出可信的全仓行覆盖百分比。

## 执行过与没执行过

执行过：环境工具链探测、Git 连通性探测（DNS 失败）、本包 mechanism_probes.py、Markdown/JSON/ZIP 生成与文件校验。

未执行：Rust/.NET 构建与回归、真实 PTY/终端、真实供应商接受/缓存/费用实验、unsafe mkfifo 错误路径。机制探针是 Python 局部移植/数学/集合核对，不替代仓库集成测试。

## 证据定位

E1：broker.rs / output.rs / fs.rs / prompt.rs / authority.rs。
E2：jcs.rs / schema_profile.rs / operation.rs / kernel/mod.rs。
E3：根 Cargo.toml / ci.yml / agent-host Cargo.toml。
E4：runtime_facts.rs 中 Unix FIFO 测试。

完整源引用及连接器 citation base 保存在 SOURCE_MAP.json；其 L2 是连接器 JSON 包装的证据行，不是源码第 2 行。下载报告中的源码链接固定到本轮 SHA。
