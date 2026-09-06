# 深入续审分流（2026-09-06）

固定源码：`12c86283b8d5991e9f17a07f14871dcf39d65066`（与上轮续审同一基线）。
原报告：[REPORT.md](REPORT.md) · [FINDINGS.json](FINDINGS.json) · [TEST_MATRIX.md](TEST_MATRIX.md) · [AUDIT_STATUS.json](AUDIT_STATUS.json)

**这不是全仓审查完成。** 审查方实际克隆失败（`Could not resolve host: github.com`），无完整 checkout、无本地 Rust 工具链。正文 29 个路径（18 全文 / 11 部分），累计两轮台账 46 路径，不是仓库文件清单。远端 CI run `33986702977` 六个 job success 不是本地重跑。本仓库未把该 ZIP 当源码包合入，也未按它重开 M15 / Chronicle / TaskGraph。

本页只做活动文档分流。活动队列现在把仍开放项排在 [NEXT_TASKS.md](../../NEXT_TASKS.md) 前列（当前 STORAGE-02），然后才回到 M16-02。哪些已在本分支修好、哪些仍在代码里，如下。这不是全仓审查完成。

## 本分支核对（2026-09-06）

审查读的是 `12c8628`。当前产品分支在该 SHA 之后已落地 F1–N2 与上轮四项修复。下列判断来自本工作树对照，不是审查方重跑。

### 审查仍视为开放、本分支已修（不重复立项）

| 审查提及 | 本分支 |
|---|---|
| 输出 EOF 后在 `select!` 分支内 `child.wait()` | F3-6a 已修：`outputs_closed` 后继续守超时/取消（process.rs / shell.rs） |
| 消费 ACK 在 `debug_assert!` 内 | F1：`c6fbbab` |
| PromptRequired 重复选入 | F1：`c6fbbab` |
| TUI resync / run_summary `required_misses.total` / shadow `duplicates_removed` | F4 续审批次已修 |

审查重读 `12c8628` 的 `process.rs` 仍看到旧 wait 路径，不能用来否定本分支修复。

### 审查针对 `12c8628` 的 CURRENT 待办（DOC-01）

`12c8628` 的 CURRENT 仍把 CI document-consistency 与 `EXECUTION_MODEL.md` 抽取写成待办。活动文档在 D0 已替换：`scripts/doc_consistency.py` 与 `docs/EXECUTION_MODEL.md` 均已存在。旧待办只留在 `docs/archive/route-reset-12c8628/`。

仍成立、且本来就写在 `state.json` 的限制：文档检查验证字段、链接、有限黑名单和 toolchain pin，**不是全部状态断言的语义一致性**。本轮只清活动入口矛盾，不建设更大文档治理系统。

### 本工作树仍在的代码问题

STORAGE-01 已在本分支落地（2026-09-06）：Windows 用 `MoveFileEx(REPLACE_EXISTING)`
替换 metadata，不再先删；缺 metadata 且存在 WAL 代际时 `RecoveryRequired`，
不铸空 g1、不选最大 `.gN`。`cargo test -p agent-storage` 覆盖残留代际与
覆盖写；未做真实进程崩溃注入。

其余项静态对照仍在，并已排到活动队列前面（当前 STORAGE-02）。Windows 崩溃、HTTP 压力、Agent crash-resume 均未在本机重跑。P1 是修复优先级，不是已发生数据损坏的事故结论。

| ID | 本树位置 | 分流 |
|---|---|---|
| STORAGE-01 | **已落地**（见上） | 未做 Windows 崩溃注入；STORAGE-02 仍开放 |
| STORAGE-02 | `compact_locked` 发布 metadata 后仍可能在 seek/切 writer 前返回 Err；`compact_authority_journal` 只转发错误，没有普通 WAL 追加那套 `failed` 围栏 | **当前工单**。普通 `append_transition` 围栏保留 |
| PROCESS-01 | `RecipeProofRunner` `host_trusted: true`，跳过 spawn 恢复登记；注释假定硬崩溃后无需回收。Linux 探针证明父进程 SIGKILL 后独立进程组子进程可存活 | 验证监督，不是模型审批。不否定 F3-6b 协作取消。慢验证走查与恢复登记一起看 |
| WORKSPACE-01 | 普通 Unix confined open 无 `O_NONBLOCK`；recovery 路径已有。`project_markers` 会同步打开 | 启动/标记探测；FIFO 名为 `Cargo.toml` 才卡住。复用 recovery 打开，`.git` 等目录标记不能一律当普通文件 |
| PACKAGE-01 | `dist.sh` / `dist.ps1` 接受 target 参数但不传 `--target-dir`；复用 `dist/<version>`。Bash 桩测：旧产物可被成功打包并进 checksum | 下次实际发布时修；干净默认 CI target 可规避部分场景。不宣称当前已发布 ZIP 已错 |
| PROVIDER-01 | Chat/Responses 非 2xx 先 `response.text().await` 再截短 | 有界错误正文；不改模型协议权威 |
| WORKSPACE-02 | 多处先 raw HANDLE，`check_not_reparse` 失败时尚未 RAII 接管。拒绝仍发生，不是路径逃逸 | 句柄所有权；复用已有 recovery helper |
| PROVIDER-02 | Chat `finish_reason=length` 无独立映射；Responses 输出上限走 typed error | provider 终止语义，原地修 |
| PROVIDER-03 | `framer.finish()` EOF 尾帧跳过 event 名与 JSON type 一致性检查 | 与正常 SSE 共用校验 |
| MCP-01 | 写请求阶段只有 deadline，取消在读阶段；部分写错误可能在 await reap 前返回 | 实验 MCP 路径；取消覆盖写/连接/读。不因此启用第二调度器 |
| PROCESS-02 | `reap` 第二次有界 wait 结果忽略后无条件 `pid=0` | 监督清理结果；生产 Unix group leader 是缓解不是证明 |
| CONTEXT-01 | `push_linked` 每实体先取桶前 64 再 newest-first；`update_entities` 用 `swap_remove`。关联边不是强制正文 Continuation | 算法升级前先修候选顺序。不宣称 GC 已误删必需上下文 |

建议回归见 [TEST_MATRIX.md](TEST_MATRIX.md)：按故障切点补进现有 crate 测试，不是新的全仓总门禁。探针脚本断言的是基线缺陷存在，修复后应改成正确性断言，不要当产品验收绿门。

## 对产品路线的约束

不重写分层，不为这 13 项新建 Planner、Chronicle、worker 或第二 Runtime。

活动队列把仍开放项排在 M16-02 之前，一次一项。已落地的 `/continue`、无头入口不回退。TUI 手工走查仍待做，不能用本审查或无头 live 代替。
