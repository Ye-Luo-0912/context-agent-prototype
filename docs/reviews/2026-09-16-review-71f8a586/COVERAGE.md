# 本轮实际读取范围

固定 SHA：`71f8a58614fcabbbc9bc9602fe85ef453df56765`。共 25 个不同文件，包含源码、测试、配置和文档。**不是全仓逐文件逐行审查完成，不计算虚假的覆盖百分比。**

“全文请求被截断”仅计实际返回证据，不把请求范围当作已读全文。GitHub 搜索片段只用于导航，目录列表不算源码阅读。某模块未列发现，也不表示该模块被证明没有问题。

| 文件 | 阅读范围 | 核对内容 |
|---|---|---|
| [crates/agent-compose/tests/cache_wire_flow.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-compose/tests/cache_wire_flow.rs) | 全文请求尾部截断；fixture、compose与断言主体已返回 | 现有真实请求key/B0/B1 smoke；read_only审批不证明成功写入 |
| [docs/reviews/2026-09-16-review-980bbc77/E1_E2_E3_E4_C_RECEIPT.md](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/docs/reviews/2026-09-16-review-980bbc77/E1_E2_E3_E4_C_RECEIPT.md) | 全文返回 | 上轮修复与 C 线关闭口径 |
| [crates/agent-contracts/src/schema_profile.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-contracts/src/schema_profile.rs) | 125–1060 多段；不是全文件 | 编译、validate、数值域、预算、strict parser |
| [crates/agent-contracts/src/jcs.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-contracts/src/jcs.rs) | 全文返回 | JCS 数值与对象编码、内联回归 |
| [crates/tool-runtime/src/tools/artifact.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/tool-runtime/src/tools/artifact.rs) | 1–850 请求；生产段1–315再次核对，测试按返回范围 | 默认参数、capture、coverage、游标与测试 |
| [crates/tool-runtime/src/tools/git.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/tool-runtime/src/tools/git.rs) | 1–440 请求的返回区间 | Git 工具结果、限制与进程调用；未作完整测试覆盖 |
| [crates/tool-runtime/src/tools/code.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/tool-runtime/src/tools/code.rs) | 1–390请求有截断；355–740；90–230、330–550再次核对 | 词法扫描、execute、结果覆盖与分页 |
| [crates/tool-runtime/src/tools/edit.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/tool-runtime/src/tools/edit.rs) | 1–330 | 编辑契约与准备路径；其余未读 |
| [crates/provider-openai/src/task_sequence_tests.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/provider-openai/src/task_sequence_tests.rs) | 1–300；不是1247行全文 | 手工ModelInput fixture与mapper序列层级 |
| [crates/agent-contracts/src/tokens.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-contracts/src/tokens.rs) | 全文返回 | Token估算辅助 |
| [crates/agent-process/src/frame.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-process/src/frame.rs) | 全文返回 | 有界帧编码与读取 |
| [crates/agent-process/src/session.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-process/src/session.rs) | 全文返回 | DuplexTransport、发送与flush |
| [crates/agent-process/src/host.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-process/src/host.rs) | 初次全文请求被截断；700–1360另读 | 调用deadline、exchange、请求/答复写入、取消、收尾 |
| [crates/agent-capability-process/src/capability_host.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-capability-process/src/capability_host.rs) | 1–360 | 生产config、invoke与broker、受管效果入口 |
| [crates/agent-workspace/src/broker.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-workspace/src/broker.rs) | 1–270 | E1剪裁标记和预算修复；不宣称全文测试阅读 |
| [.github/workflows/ci.yml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/.github/workflows/ci.yml) | 全文返回 | Linux分片与host加入、既有CI命令 |
| [Cargo.lock](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/Cargo.lock) | 1750–1880、1950–2070 | Ratatui等版本与Ryu1.0.23；未全审依赖 |
| [crates/agent-runtime/src/execution/memo.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-runtime/src/execution/memo.rs) | 全文返回 | 预留memo始终miss、未接dispatch |
| [Cargo.toml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/Cargo.toml) | 全文返回 | 20workspace成员与共享依赖 |
| [crates/agent-runtime/src/output.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-runtime/src/output.rs) | 全文返回 | E1兜底剪裁完整性标记和回归 |
| [docs/NEXT_TASKS.md](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/docs/NEXT_TASKS.md) | 1–190请求被截断；仅返回的当前入口/历史段 | 当前范围与已关闭项；不把截断当缺失 |
| [crates/agent-tui/src/cli.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-tui/src/cli.rs) | 1–690 | Headless入口、JSONL队列、取消、结果分类；非全TUI |
| [crates/agent-workspace/src/runtime_facts.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-workspace/src/runtime_facts.rs) | 255–末尾 | E4 CString/FIFO fixture修复 |
| [crates/agent-core/src/kernel/mod.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-core/src/kernel/mod.rs) | 975–1160、1480–1610 | Schema执行门禁、恢复与回滚；不是整个Core |
| [crates/agent-compose/tests/proof_supervision.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/71f8a58614fcabbbc9bc9602fe85ef453df56765/crates/agent-compose/tests/proof_supervision.rs) | 全文返回 | 真实host死亡与验证树退出断言、CI失败点 |

## 额外观察

读取了 commit compare、main（收尾仍为固定SHA）、CI run、七个job结果及Windows失败job原始日志。原始日志读取不是本地执行。历史review有些源码证据仍在对话中，但没有作为本轮新增通读计数。

## 没有执行

本环境没有 Cargo/Rustc/.NET；Git fetch DNS失败。没有 Rust/.NET/终端/真实供应商测试、没有写入仓库。仅运行随包机制移植脚本与Node参考比较。
