# 实际覆盖与执行记录 — 3bdb269c

## 口径

本轮是全仓范围的续审，不是全文件逐行通读。读取了根目录/20 个 workspace crate 的结构，逐项核对 d92564bc 到 3bdb269c 的变更清单；正文阅读集中在下表。目录树、文件大小、GitHub 搜索片段都不算全文阅读。

本轮不把上一轮 d92564bc 的 TUI 全文阅读直接升级为“最新 TUI 全文已读”。修改过的文件只按本轮实际区间记录。未列出的 SDK、GUI、脚本、测试和其他生产文件，不在本轮全文覆盖声明内。

## 正文范围

| 文件 | 本轮实际阅读范围 | 用途/限制 |
|---|---|---|
| docs/CURRENT.md | 当前基线、批次状态与限制部分 | 版本/旧项核对；不把实现回执当运行证明 |
| docs/NEXT_TASKS.md | 当前入口与接手顺序；请求 1–105，长输出存在截断，只采信可见部分 | B1/B2、U 系列关闭状态、活动入口重复历史 |
| crates/agent-tui/src/state.rs | 1–235、350–1120、1260–1950，以及已有返回中的相关片段 | 事件身份、重放、快照、结果统计；其余内联测试未全文复核 |
| crates/agent-tui/src/session.rs | 1–640、830–1030 | worker、审批循环、命令分流；其余测试未全文复核 |
| crates/agent-tui/src/cli.rs | 330–610 | headless 缺口/关闭/取消；非全文 |
| crates/agent-tui/src/main.rs | 1–295 | 终端 guard、panic hook、启动前段；非全文 |
| crates/agent-tui/src/ui.rs | 1–320 | 实际布局、宽度、滚动；非全部测试 |
| crates/agent-runtime/src/sink.rs | 全文（再次按固定 SHA 获取） | 实时事件的真实 envelope 语义 |
| crates/agent-runtime/src/status.rs | 1–290 | 公共读模型；非全部测试 |
| crates/agent-runtime/src/actor/model.rs | 1600–1815 | ModelStarted → LiveSink → provider → outcome |
| crates/agent-runtime/src/actor/tools.rs | 1–265、710–1030 | 操作准入、stale settlement、补账 |
| crates/agent-runtime/src/actor/turn.rs | 1–340、2760–3050、3300–3645；另从完整返回资源扩展 proof-refresh/完成机会片段 | 完整 fetch 输出被截断，绝不因此计作全文；资源行号不冒充源码行号 |
| crates/agent-replay/src/run_summary.rs | 1–370 | 离线聚合；非全文/非运行验证 |
| crates/context-baselines/src/rolling.rs | 全文（1–300、301–630、631–文件尾连续读取） | 扩展到未改动的基线实现；未执行并发或性能实验 |

## 元数据覆盖

- main 初始和收尾 SHA，均为固定审查版本。
- d92564bc...3bdb269c 提交比较：9 个提交；生产变化集中在 TUI 与 StatusProjection。
- 根 tree、crates tree（20 crates）、agent-replay/context-baselines 目录树。
- 该 SHA CI run 35107501790，attempt 1 success。

## 外部参考

读取 Ratatui 上游 reflow 的 WordWrapper 部分（当前 main，返回 blob 21072925405d6ed00f0fc973ca254de128423374）、Paragraph 官方 API、unicode-width 官方文档、OpenAI 官方缓存文档。它们用于语义核对，不计入用户仓库正文覆盖；实际兼容性回归须使用仓库锁定依赖。

## 执行记录

| 项目 | 结果 |
|---|---|
| GitHub 连接器固定 SHA 读取 | 已执行 |
| 远端已有 CI 状态查询 | 已执行，当前 SHA success |
| 本地 git 访问 | 实际尝试，DNS 解析失败 |
| 源代码归档获取 | 未得到可用归档 |
| cargo/rustc/dotnet 可用性 | 本轮探测未找到 |
| 新 Rust 回归/全仓本地测试 | NOT_RUN |
| 实际 PTY/手动终端旅程 | NOT_RUN |
| 真实供应商 API/KV/付费成本实验 | NOT_RUN |
| GitHub 修改/提交/推送 | 未执行 |
| 本报告/任务文档生成 | 已执行 |

本报告的“确认”指代码控制流或类型/身份关系可直接推出，不是运行回执。并发/时间顺序案例给出可控的测试接缝，避免用 sleep 或扩大超时制造绿色。
