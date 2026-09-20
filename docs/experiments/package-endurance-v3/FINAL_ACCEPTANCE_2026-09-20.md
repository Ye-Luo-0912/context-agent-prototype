# Package 优化：最终集成验收

2026-09-20，用户要求继续完成整个优化任务。本回执接续
[最终验收计划](FINAL_ACCEPTANCE_PLAN.md)，与此前
[v3 实验回执](RUN_2026-09-20.md) 和 [补充启动清理回执](FOLLOWUP_VALIDATION.json)
分别保存。HEAD 为 `71322074596b7e604be5c8750d350e7d626b0e28`，本轮代码仍在
`codex/runtime-endurance-full-plan` 的未提交工作树。

**状态：本轮优化实现与全部计划内本地验收完成。** 最终长负载、静止库审计与
其余集成检查均实际通过，详见 [总回执](final-evidence/SUMMARY.json)。新验证全程零付费，不重置原模型额度，
不改旧 campaign、供应商账本、模型原稿或冻结 evidence。

## 已收口的实现

| 用户动作 / 实际故障 | 最终行为 | 代码与验证 |
| --- | --- | --- |
| 合法长中文目标或长纠正被 host 的 4096 字节预解码限制断连 | 帧与结构预算保持，正文交给类型化业务校验；超限返回错误且连接可继续用 | [host 解码](../../../crates/agent-host/src/lib.rs)、[解码回归](../../../crates/agent-host/src/decode_tests.rs)、[真实传输测试](../../../crates/agent-host/tests/host_e2e.rs) |
| 工具 A 已提交，工具 B 被取消后，A 的观察随 TurnFrame 丢失 | 两种取消入口在丢 frame 前保全已接受观察；Committing 不重复 ingest；失败围栏并保住费用事实 | [Runtime](../../../crates/agent-runtime/src/actor/turn.rs)、[7 项组合回归](../../../crates/agent-runtime/tests/turn/combined_cancel.rs) |
| 保全一半时第二条 observation 失败 | 第一条只写一次；返回 RecoveryRequired，无成功取消终态；重复取消/继续不再 ingest，迟到 prepared 效果 rollback | 新增两条公开入口动态回归，原来仅静态推导的缺口已补齐 |
| 阶段间受保护文件被改后重新当成合法基线 | 有 files 清单的 campaign 在启动前核对原始 SHA，结束后再次核对；旧无清单流程保留兼容范围 | [runner](../../../scripts/runtime_endurance_incremental_runner.py)、[完整性回归](../../../scripts/tests/test_runner_integrity.py) |
| 父进程先退出，或 Job 计数归零早于 HANDLE 退出 | 保留 Job 所有权及 PID/创建时间绑定的原生句柄；历史覆盖、总数稳定、所有句柄退出、直接进程 wait 同时满足才确认 | [helper](../../../scripts/runner_process_tree.py)、[生命周期回归](../../../scripts/tests/test_runner_lifetime.py) |
| 创建进程后的启动与清理同时失败，构造函数未返回 | 异常携带进程及清理事实，runner、Worker、OwnedHost 均接回并重试收尾；保留 CLEANUP_UNCONFIRMED，不写成 no_process_started | [负载调用方](../../../scripts/package_endurance_v3/load.py)、[host 调用方](../../../scripts/package_endurance_v3/journey.py)、[6 项调用方回归](../../../scripts/package_endurance_v3/tests/test_controller_guards.py) |
| Job 枚举遇到一次正常成员变化后永久停止观察 | 仅两类类型化成员快照变化允许每 pass 最多 3 次、共享原 deadline 的重采；其他 native 错误仍 sticky；最终确认条件不变 | 生产捕获函数反例及受控真实成员增长先红后绿，6 项新回归；独立定向 review 无确定新缺陷 |
| Ubuntu 只有 python3，没有 python 别名时，L0 无法启动 | 默认使用正在运行本脚本的 sys.executable | [campaign](../../../scripts/runtime_endurance_full_campaign.py)、[CLI 回归](../../../scripts/tests/test_campaign_guard.py)；Ubuntu 实际复现并复验通过 |
| 新 Python 回归没有在 CI 执行 | 既有 workflow 增加 Windows/Ubuntu × 两套 unittest 矩阵，不依赖 Rust 构建、不调用付费模型 | [CI](../../../.github/workflows/ci.yml)；本机两平台实际测试，远端运行结果另记 |

人工修复应用与 auditor 的来源、契约和限制继续见
[应用说明](../../../scripts/package_endurance_v3/fixture/app/README.md) 和
[auditor 说明](../../../scripts/package_endurance_v3/auditor_assisted/README.md)。
它们分别修复历史重放回退、worker 参数/回执、未知发布盲重试，以及审计漏报与
未转义 SQLite URI 写入。原模型严格验收仍为 3/13，不能由人工修复反写成自主成功。

## 验证矩阵

| 实际验证 | 结果 |
| --- | --- |
| Windows `cargo build --workspace` | 通过 |
| Windows `cargo test -j 2 --workspace -- --test-threads 2`，最终完整复验 | 退出 0，3066 passed / 10 ignored；首次失败保留在下方 |
| Linux/WSL `cargo test --locked -j 2 -p agent-runtime -p agent-host -- --test-threads 2` | 退出 0，822 passed / 1 ignored，包含 Unix host 与全部 7 项取消组合回归 |
| Windows `cargo clippy --workspace --all-targets -- -D warnings` 与全仓 fmt | 通过 |
| Windows `python -B -m unittest discover -s scripts/tests -v` | 58/58 |
| Ubuntu 同套 Python runner 测试 | 58 总数，39 执行通过，19 个 Windows 原生用例跳过 |
| Windows v3 unittest | 40/40 |
| Ubuntu v3 unittest | 40 总数，39 执行通过，1 个 Windows 原生用例跳过 |
| Windows `dotnet build apps/Agent.Desktop/Agent.Desktop.csproj` | 通过，0 warning / 0 error；本次进程设置遥测退出选项 |
| Windows `dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj --no-restore` | 139/139，包含真实 host 链 |
| 文档、Linux Rust 分片覆盖、diff 检查 | 通过 |
| 当前源码的独立应用验收 | 11/11 |
| 最终 helper 的 host 预检与同库负载中 host 旅程 | 各 7/7，真实工具/进程，合成供应商；四宿主 cleanup 均确认 |
| 最终 helper 的完整 1800 秒负载及结束后静止审计 | 1794 批、7176 次安装校验；两次真实进程死亡恢复；全部 7177 条持久回执通过静止审计 |

Rust 的平台验证是本机 Windows 与 WSL Ubuntu；远端 GitHub CI 没有运行。
不把 Python 的平台跳过计为实际测试，也不把 Linux 定向两 crate 写成全 workspace。

## 本轮失败、诊断与修复来源

1. 受限环境中的 host 客户端实际收到 Named Pipe `PermissionError`，Rust 的 MCP
   测试也因无法设置临时目录完整性标签失败。保留原日志；通过工具审批在原生
   权限环境执行同一验收。没有通过放宽 Core 或删测试绕过权限边界。
2. 第一次原生完整 Rust 测试的 `agent-process::supervisor::an_unobservable_kill_reports_unconfirmed`
   在 Drop 后固定 400ms 的 PID 观察处失败。精确测试、受影响 lib 串行、原 workspace
   同一二进制精确/串行均复验通过，之后整个 workspace 再跑通过。原谓词把运行中和
   无法观测都返回 true，且未绑定创建身份；因此只记录瞬态退出观测失败，不能断言
   一定是 OS 调度抖动。未修改该生产模块。直接运行测试二进制时的一次 helper
   查找环境缺失也保留记录，提供 Cargo 等价环境后通过。
3. 首个新 host 运行只采到历史进程 7 个中的 3 个，保持清理未确认。隔离反例证实
   observer 被一次成员变化永久终止；修复后保留全部确认门。旧负载在约 488 秒时
   中止，最后回执仍为 RUNNING，不把它算作 PASS；单列中止记录，并经获批只读
   进程枚举确认该目录没有存活 Python/host。最终版本在新目录完整重跑。
4. 首次 Ubuntu runner 测试因缺少 `python` 别名失败；修复默认解释器路径后，
   定向与全套通过。没有为适配测试修改系统别名。
5. 首次桌面构建无法写沙箱外 Avalonia 遥测日志；仅为本次命令设置
   `AVALONIA_TELEMETRY_OPTOUT=1` 后通过。一次 .NET 自动 restore 未前进而被中止，
   最终使用已有依赖 `--no-restore` 执行了全部 139 项测试。

## 最终负载边界

最终输入位于 `target/package-optimization-final-20260920-r2/`，与失败及中止的
`target/package-optimization-final-20260920/` 分开。两个窗口均声明付费上限 0。
最终 helper SHA256 为
`b6830875c8a962bb091b2a5679714c736735d51318e4aeb9eb1723d19df842e7`。
host 使用与 CI 一致的显式 Python 解释器；预检用独立 stage，负载中的正式 host
只运行一次，避免固定 client request ID 把重跑误当新鲜旅程。

正式轨迹是 **4 个应用 worker 与 GC 的持续负载，加同库的短 host 连续旅程**。
它不等于 Rust Runtime 所有场景持续 30 分钟。取消 ACK 与进程实际退出分别观察，
清理完成只来自保留 HANDLE 的退出事实。结束后通过公开应用入口恢复，再对
静止库作全部历史回执和文件字节核对；不删除侧文件来制造可审计状态。

实际执行 1794 批，4 个 load tenant 各有 generation 1–1794 的 1794 条回执，
另有一次正式 host 的 runtime/prod 回执，总计 **7177**。时间线独立复算 1794 批
均发生安装与 GC 重叠；首次安装开始至最后安装结束 **1799.005 秒**，控制器
目标窗口为 1800 秒，含全历史扫描及清理的总耗时 **1844.343 秒**。GC 实际删除量
单列在总回执，不将本轮视为大量垃圾删除吞吐证明。

7 份工作进程清理和 4 份正式 host 清理回执均满足完整身份与 HANDLE 退出条件。
静止审计实际核对 7177 条回执，审计前后 **183 个文件字节不变**；最终 13 项
冻结输入摘要全部一致。审计结束后再作获批的只读进程枚举，该验证目录没有
残留 Python/host 进程。

证据入口：[测试结果与实际命令](final-evidence/TEST_RESULTS.json)、
[最终负载](final-evidence/load-30min.json)、
[独立历史库核验](final-evidence/final-repository-verification.json)、
[源码身份](final-evidence/SOURCE_IDENTITY.json)、[归档说明](final-evidence/README.md)、
[逐文件哈希](final-evidence/MANIFEST.json)。完整测试日志以 gzip 保存；原失败、中止
与修复前反例同样保留。旧 `evidence/MANIFEST.json` 及 `FOLLOWUP_VALIDATION.json`
保持原有版本含义。

停止条件已满足，不再扩大该切片。修改未提交/推送；GitHub 远端 CI 未运行。
原模型自主审计器的严格验收失败、供应商布局/费用 A/B、恶意并发快照替换等不因
本轮本地通过而改判。OperatorClosureOnly 的完成语义、Core 权限与恢复围栏保持。
