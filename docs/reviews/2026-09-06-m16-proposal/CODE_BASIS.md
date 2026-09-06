# 代码依据与未完成范围

## 实際访问

仓库：`Ye-Luo-0912/context-agent-prototype`；连接器在本轮返回的main提交：`12c86283b8d5991e9f17a07f14871dcf39d65066`（2026-09-05T19:18:10Z）。

本轮实际执行clone，退出128：`Could not resolve host: github.com`。codeload的直接访问同样DNS失败；另一下载入口没有取得归档。本地可运行git/Python，但没有cargo/rustc。

因此：**完整checkout未取得；所有代码逐行审查未完成；本轮无Rust构建/测试、无真实LLM任务、无新算法对照；没有修改远端仓库。**不沿用附加历史报告中的“全工作区实际测试”作为本轮结果。

本轮读取13个不同源码/文档路径，其中6个返回全文，7个只返回指定段。一次runtime递归tree返回截断，不能充当完整文件清单。`docs/NEXT_TASKS.md`在此commit返回404；本包给的是待合并的新替换稿。路径表、脚本盘点和hash均不等于完成语义审查。

## 本轮来源

| ID | 路径与链接 | 读取范围 | 用于什么判断 |
|---|---|---|---|
| S01 | [AGENTS.md](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/AGENTS.md) | 全部返回 | 当前依赖边界、默认工作集约定和历史冻结条款 |
| S02 | [crates/agent-runtime/src/command.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-runtime/src/command.rs) | 全部返回 | 公开 continuation、任务操作、Core query、取消与停止入口 |
| S03 | [crates/agent-runtime/src/task.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-runtime/src/task.rs) | 1–340 | TaskRecord、TaskAnchor、完成策略、进度与验证修订 |
| S04 | [crates/agent-runtime/src/instance.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-runtime/src/instance.rs) | 全部返回 | 跨平面 checkpoint/restore 与有序 shutdown |
| S05 | [crates/agent-tui/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-tui/src/main.rs) | 1–305 | 参数解析、启动顺序、实际产品 ComposeConfig、事件入口 |
| S06 | [crates/agent-runtime/src/actor/commands.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-runtime/src/actor/commands.rs) | 1–300 | 任务两阶段激活、只读列表、空闲约束、anchor CAS |
| S07 | [crates/tool-runtime/src/verification.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/verification.rs) | 1–330 | TaskScoped/ExactCurrentWorld、source_read_only 与 host coverage |
| S08 | [crates/agent-compose/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-compose/src/lib.rs) | 1–300 | 可复用组合、ContextPolicy、严格模型配置和 profile |
| S09 | [docs/state.json](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/docs/state.json) | 全部返回 | 已记录 alpha/release、现行队列与陈旧待办同时存在 |
| S10 | [crates/agent-runtime/src/actor/turn.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-runtime/src/actor/turn.rs) | 300–490 | 实际轮数计量、RoundBudget 退出、batch settle、terminal completion |
| S11 | [crates/agent-compose/tests/product_flow.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-compose/tests/product_flow.rs) | 全部返回 | 测试采用的配置、正常 shutdown/restore、未调用 continuation |
| S12 | [crates/agent-compose/tests/crash_resume.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-compose/tests/crash_resume.rs) | 1–220 | 已有真实硬退出测试与变更 journal 计数，不重新发明 crash harness |
| S13 | [crates/tool-runtime/src/tools/task_manage.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/tools/task_manage.rs) | 全部返回 | catalog-cold 的进度工具、RuntimeDirective、字段级 CAS 与权限排除 |

## 需纠正或避免沿用的假设

1. 默认任务不是可以凭模型一句“完成”就durably close；OperatorClosureOnly与EvidenceRequired分开。
2. next_action不是必须为空的完成阻塞，计划勾选也不代表验证完成。
3. task.manage不是所有轮次常驻工具，work入口需通过现有工具需求接入。
4. max_tool_rounds命名与实际model_round判断不同，必须按真实计量对外说明。
5. product_flow.rs不能单独证明“正常产品配置+强杀+恢复后continue”已全部覆盖：源码里配置不同、正常shutdown、后半段未调用continue。真实crash_resume测试另外存在，不能因此说全仓没有崩溃测试。
6. RuntimeInstance已经处理跨平面恢复和有序关闭；不能把附件早期“需要建这些”重新作为从零开发列表。

## 历史材料的地位

- 用户附件《审查仓库完整性.txt》：作为历史设计提案，其Chronicle/TaskGraph路线没有自动变为本阶段要求，历史试验/测试结果未重验。
- agent-audit-20260906-deep/FINDINGS.json：13项问题原样作为carry-forward来源。其静态、OS探针、脚本桩测试等证据等级不提高；没有标成全Rust复现。
- agent-audit-12c86283-continued/replacements/docs/NEXT_TASKS.md：旧活动队列提案，由本包收敛替换，不并行执行。

## 不是本轮已排除风险的部分

未逐文件覆盖的Core/kernel、storage实现尾部、Runtime执行/恢复/检查点全部逻辑、Context引擎及所有测试、provider完整协议和重试、全部扩展/服务、所有eval/evidence与脚本，均未在本轮证明正确。全工作区模块地图只说明拟议责任，并不宣布这些实现安全或完成。

执行者取得完整checkout后，应先盘点全部tracked文件，再完成当前切片的实现/调用方/测试阅读；未读文件保持OPEN。不能把运行过清单脚本改称“查看了全部代码”。
