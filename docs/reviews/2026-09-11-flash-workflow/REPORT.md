# DeepSeek Flash：实际工具链首轮走查

2026-09-11；HEAD `732cf93103cb7104f1961402f422e82143b8956c` 加已有工作树改动。当前工作树构建 `agent-tui`，350 个 Rust 源码/构建文件摘要在运行前后相同，二进制摘要已保存。所有模型请求配置为官方 `deepseek-flash`、Responses、`reasoning.effort=none`、原生默认缓存、dynamic Context。

收尾时工作区同期已推进到 `f4dda3b45a1cf2dd80c1054da7d4a7d8795dd849`（不是本执行者发起的提交），350 个源码摘要仍完全相同。当前 `target/debug/agent-tui.exe` 在全部运行结束后被重建，SHA 已不同；本证据绑定运行前记录的二进制 SHA，不把当前 exe 当作当时工件。原二进制未另行保存，复核以已保存源码/运行身份和日志为限。[收尾工作区核对](workspace_after.json)。

**固定的三个小型隔离仓库样本均通过预定产物检查。** 代码来自本仓库的文档校验脚本；这不是大型整仓开发验收，也不证明相对旧版本降本。初始任务、权限、上界和不覆盖内容已在付费调用前写入 [TASKS.md](TASKS.md)。样本输出留在证据目录，没有应用回用户的生产工作树。

## 实际结果

| 样本/执行段 | 结束 | 已报告模型调用 | 输入 / 输出 tokens | 子进程总耗时 |
|---|---|---:|---:|---:|
| A 跨模块提取 | exit 0；9 类测试、真实委托、修改范围均通过 | 7 | 39,857 / 1,649 | 12.48 s |
| B 九类应用检查 | exit 0；验证结果 9/9，与实际 pytest 输出一致，源文件未改 | 6 | 31,258 / 1,233 | 11.46 s |
| C 首段读后让出 | exit 2；round_budget，落下可验证快照 | 1 | 2,380 / 87 | 4.14 s |
| C 冷恢复后超时取消 | exit 1；timeout，持久日志含 TurnCancelled | 1；另有 1 个已启动操作被取消、无 usage | 2,617 / 79（已知部分） | 3.74 s |
| C 第二次冷恢复并交付 | exit 0；尾部标记与九类通过数正确，只写一次结果 | 6 | 29,726 / 859 | 11.44 s |

A 将校验逻辑提取到新模块，旧入口调用它；模型之后真实执行 `verify.run` 并查看 diff。另做本地委托检查和修改范围核对，测试文件保持原样。[diff](evidence/a/final.diff)、[新模块](evidence/a/deliverable/scripts/doc_links.py)、[可信验证](evidence/a/verification.json)。

B 的 [validation.json](evidence/b/deliverable/validation.json) 与真实测试对应，原脚本及测试都未修改。这里是九类可执行应用检查，**没有九份独立 host 覆盖声明**，不能替代 W05 的九域证明保留实测。[可信验证](evidence/b/verification.json)。

C 两次 `RuntimeRestored` 后沿原指令执行；4,065,097 字节、32,019 行的真实工具输出保存在工件中。模型只调用一次 `artifact.read`，读取第 31,995–32,019 行（25 行），返回正文 3,006 UTF-8 字节并包含末端标记；随后一次写入 [summary.json](evidence/c/deliverable/summary.json)。源码、测试未变，三个进程没有强制杀进程树，取消段没有发生输出写入。诊断测试只执行一次。[事件](evidence/c/c-resume/events.jsonl)、[取消持久日志](evidence/c/c-cancel/journal.jsonl)、[可信验证](evidence/c/verification.json)。

## 成本、交互性与资源

- 22 个模型操作启动，21 个完成并报告 usage：输入 **105,838**、输出 **3,907**、缓存读取 **72,704** tokens。21 个完成调用均报告单次尝试、零重试。取消操作的 usage 未返回，不能把它计作零费用；没有读取实际账单。这些数值不等于完整账单或 HTTP 发送次数。
- 五段子进程耗时合计 **43.27 s**，包括各段启动、模型、工具与停机；不含构建、依赖安装和独立事后验证。工具 `ToolStarted→ToolFinished` 合计 3,485 ms（含该区间内提交/事件开销），单次最大 1,376 ms。
- 44 次已完成维护报告均无 compaction，`deferred_folds=0`。本组未触发模型折叠，不能据此声称真实供应商慢维护取消已覆盖；此前 W04 门控回归是另一层证据。
- C 配置 1 秒事件等待上限后记录 timeout 和 TurnCancelled。产品没有发出精确“取消请求开始/类型化回执返回”时间，故精确延迟为 **不可测**。3,742.83−1,000 = **2,742.83 ms** 仅是包含启动/收尾的宽松上界，不冒充取消 ACK 实测。
- 观察到的 Context resident 峰值：A 18,259 B；B 14,378 B；C 21,020 B。活跃条目峰值分别 4、6、5。最大工具模型正文为 12,220 UTF-8 字节，约 4 MB 原输出通过工件引用传递。没有采集进程树 RSS，不能把 Context 字节当全进程内存。
- A/B 未出现相同 revision、相同窗口重复读取。C 的 README 和测试定义在三段各读一次，合计四次重复读取；这是实际恢复成本，任务首段读要求与原指令回放也参与其中，不能直接归因于 Context 遗忘。未出现失败写入后的重复尝试；验证没有因证明淘汰而反复执行。

完整机器可读汇总：[audit.json](evidence/audit.json)。来源和二进制身份：[manifest.json](evidence/manifest.json)。56 个证据文件约 4.95 MB，含完整大输出和文件摘要：[files.json](evidence/files.json)。工作区原件位于 `target/flash-workflow/1789058357423842400/`。

## 发现与界限

恢复段仍将 `session_end.task_state` 报为 `none`，尽管继续任务成功、没有 TaskCompleted，且原活动任务继续保留。这复现了 [已有 backlog](../../AUDIT_TODO.md) 的无头恢复状态低报；没有把它算作“任务已持久完成”。A/B 普通 final 为 `awaiting_operator_review`，本轮没有授予额外自动关闭权。

**下一优先片为 dynamic ingest 的取消边界。** 执行后核对源码发现，`SimpleContextEngine::ingest(UserMessage)` 在 episode 轮换时可直接 await `run_distill`，而 Runtime 的 `prepare_user_message` 仍内联 await ingest。上一片只把 `ContextEngine::maintain` 拆入 operation，不能据此声明这个真实供应商等待也可取消。调用链已读，实际门控反例尚未执行；本组没有轮换压缩，不能用于关闭该缺口。下一片先用真实 Simple 引擎加门控 compactor 复现，再沿既有输入事务、join 与快照回滚修复，不新建调度器或原样补跑 live。

这轮覆盖小型真实代码样本的工具循环、应用检查与 Windows 跨进程恢复；仍未覆盖大型跨 crate 修改、九份独立 host 验收声明、真实 compactor 长等待、精确控制 ACK 延迟、其他平台及旧版本同任务对照。后续先补上述 Runtime/Context 边界，再沿原队列补实测缺口，不重复本组成功调用、不重开 M15/LT-EVAL。

## 执行与数据修正

1. `cargo build -p agent-tui`：通过，用当前工作树构建。
2. 在 `target/flash-workflow-env/` 创建独立 Python 环境并安装 pytest 8.4.2。九类起点测试 9/9；demo 模式确认产品入口、授权 JSON 和采集可用，demo 不计入真实调用。
3. 官方 `/models` 只读预检确认请求的 `deepseek-flash` 可用。用户先前提供的凭据仅经进程输入/环境传入，未进入源文件、fixture、报告或授权文件。
4. `python -B docs/reviews/2026-09-11-flash-workflow/run.py` 执行 A/B 后，汇总脚本因 Windows 路径键使用反斜杠而抛 KeyError。模型任务本身均正常结束。保留 [原 runner](evidence/runner.initial.py)，修正路径规范化并加强不可修改文件检查，离线重验 A/B；`--resume target/flash-workflow/1789058357423842400` 只执行尚未开始的 C，没有重跑 A/B。修正版本和原因载于 manifest。
5. 三组事后 pytest 均 9/9，A 另验证旧入口确实委托新模块；源码/测试和额外文件修改范围核对通过。C 额外核对完整工件 SHA-256、尾页内容、两次恢复、类型化取消、单次输出写入。证据包扫描未发现 credential-shaped 内容。
6. `python scripts/doc_consistency.py` 与 `git diff --check` 通过；运行脚本 AST 解析、56 个已保存证据摘要和 350 个源码摘要复核通过。最终二进制相等检查发现上述运行后重建，已单独记账，没有覆盖原摘要或补跑 live。生产代码未在本轮修改，未重复上轮 615 项 Runtime 回归。

本轮没有修改生产 Rust/文档校验脚本、提交、推送或启动子 agent。只新增可审阅运行计划、薄运行脚本与本次证据；历史冻结结果保持原样。

后续记录（2026-09-11）：上述 dynamic ingest 等待已由独立 Runtime 切片复现并修复，真实 Simple 引擎门控取消/停止/正常完成及 Runtime 620 项回归通过，见 [输入压缩取消回执](../2026-09-11-ingest-cancellation.md)。该修复不改本报告的执行源码、二进制摘要、原始数据或覆盖范围，也没有补跑本组成功的 Flash 请求。
