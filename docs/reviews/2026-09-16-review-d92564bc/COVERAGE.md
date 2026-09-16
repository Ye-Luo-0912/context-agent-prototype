# 实际读取范围

固定提交 `d92564bcfa41dda44f752e1e49e88a321abdf942`。本表只记录本轮，不将以前提交的读取自动记为当前覆盖。

TUI 的八个 src 文件（含内联测试）已读到文件结尾；独立启动测试及 crate 配置全文已读。全仓其他部分是调用链续审，**不声称已经逐行读完全部代码**。

范围中的上界是请求窗口，不代表文件实际行数；“读至 EOF”表示返回内容已包含文件结尾。一个文件的“全文读取”不是“没有问题”的证明，更不是测试通过。

| 文件 | 读取覆盖 | 用途/限制 |
|---|---|---|
| [Cargo.toml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/Cargo.toml) | 全文 | 工作区 20 crate 及依赖版本 |
| [crates/agent-tui/Cargo.toml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/Cargo.toml) | 全文 | TUI 依赖/测试依赖 |
| [crates/agent-tui/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/main.rs) | 全文（1–520 窗口读至 EOF） | 启动、审批组合、终端设置/清理、headless |
| [crates/agent-tui/src/args.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/args.rs) | 全文（1–290；291–650 读至 EOF） | 参数、默认配置、内联测试 |
| [crates/agent-tui/src/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/work.rs) | 全文 | 共享长任务入口薄适配 |
| [crates/agent-tui/src/ui.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/ui.rs) | 全文 | 真实 render、Line/Paragraph、视窗/光标、测试 |
| [crates/agent-tui/src/session.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/session.rs) | 全文（1–945；946–1300；1301–1700；1701–2300 读至 EOF） | 事件循环、命令分发、保存/恢复、CaptureSink/E2E 与有界读取测试 |
| [crates/agent-tui/src/state.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/state.rs) | 全文（1–1540；1541–1900；1901–2350 读至 EOF） | 状态折叠、审批、review、日志重放、全部内联测试 |
| [crates/agent-tui/src/cli.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/cli.rs) | 全文（1–620；621–1030；990–1340；1341–1850 读至 EOF） | 生产 drain/JSONL 与全部内联测试；第一次尾部截断由后续重叠读取补齐 |
| [crates/agent-tui/src/doctor.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/src/doctor.rs) | 全文（1–470；471–850 读至 EOF） | 检查计数、脱敏导出和内联测试 |
| [crates/agent-tui/tests/real_binary_startup.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-tui/tests/real_binary_startup.rs) | 全文 | 真实二进制 preflight 和 headless demo |
| [crates/agent-runtime/src/status.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-runtime/src/status.rs) | 1–290 | 公共投影完整生产实现及测试开头；后续测试未读 |
| [crates/agent-runtime/src/instance.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-runtime/src/instance.rs) | 1–270 读至 EOF | RuntimeInstance/CheckpointPlane/ordered shutdown |
| [crates/agent-runtime/src/command.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-runtime/src/command.rs) | 1–250；290–490 | 命令身份、handle Sender 所有权、expecting/reporting；非全文 |
| [crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-simple/src/engine.rs) | 1–100；830–1470；2090–2430；3220–3305；3440–3760 | 原子搜索、导出、spill、required 解析与材料化、checkpoint；非全文 |
| [crates/context-simple/src/materializer.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-simple/src/materializer.rs) | 1010–1465 | required 计划与冷解析失败分类；非全文 |
| [crates/context-contextcore/src/adapter.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/context-contextcore/src/adapter.rs) | 285–495 读至 EOF | 协商后原子结果/续查、旧服务拒绝、恢复尾部；前部未通读 |
| [crates/provider-openai/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/provider-openai/src/lib.rs) | 840–1125 | 当前缓存内容块放置规则与真实 wire 构建部分；非全文 |
| [crates/agent-compose/tests/cancel_usage_settlement.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/crates/agent-compose/tests/cancel_usage_settlement.rs) | 全文 | 已知取消 usage 正式事件回归；只读未运行 |
| [docs/NEXT_TASKS.md](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d92564bcfa41dda44f752e1e49e88a321abdf942/docs/NEXT_TASKS.md) | 返回摘录（全文请求被输出截断） | 只作为当前队列定位；不把未见后半部当作已读 |

## 本轮没有声称覆盖的部分

其余 crate 全部实现、完整 SDK/.NET 客户端、GUI、全仓脚本与所有集成/平台测试，没有在本轮重新逐行读取。GitHub search 命中仅用于定位，不算相应文件全文读取。上轮已经完整给出的事实作为历史背景，不替代当前代码核对。

## 执行状态

远端 CI success（attempt 1）已经实际读取。本地 git 访问 DNS 失败；cargo/rustc/dotnet 缺失。没有运行 Rust/.NET 回归、实际 Ratatui/PTY、用户命令或付费供应商请求。源码推导的反例需在执行工作树加入并运行。

没有修改/推送仓库。本包文件只生成在会话工作容器中。

## 核对的外部语义

Ratatui 0.30 Line 构造/转换移除换行；手工终端管理需要显式恢复/异常路径。来源为官方 Rust API 文档与 Ratatui recipe，非截图推测。JSONL/drain/锁所有权等仓库事实以固定版本源码为准。
