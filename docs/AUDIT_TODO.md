# 缺陷分流

当前执行顺序由 [NEXT_TASKS.md](NEXT_TASKS.md) 前列决定：先关闭 2026-09-06 深入续审仍开放项（当前 **STORAGE-02**），再回到 M16-02。
不把 13 项清零设成新阶段，不要求全部关闭才允许以后交付功能；但本工作树现在把仍开放项排在产品剩余之前。

原报告与探针：[reviews/2026-09-06-deep-audit/REVIEW.md](reviews/2026-09-06-deep-audit/REVIEW.md)。
建议回归按 [TEST_MATRIX.md](reviews/2026-09-06-deep-audit/TEST_MATRIX.md) 补进现有 crate，不新建总门禁。
旧审计正文：`docs/archive/route-reset-12c8628/docs/AUDIT_TODO.md`。

## 仍开放（活动队列前列）

审查在 `12c8628` 上静态确认；本分支对照后仍在。均未在本机用真实 Windows 崩溃、HTTP 压力或 Agent crash-resume 重跑。P1 是修复优先级，不是已发生数据损坏的事故结论。

| 工单 | 已核对位置 | 要修什么 | 不要做什么 |
|---|---|---|---|
| **STORAGE-02（当前）** | `crates/agent-storage/src/lib.rs` `compact_locked`；`persist_authority_metadata` 成功后仍可能在 seek/切 writer 前返回 Err。`agent-core` `compact_authority_journal` 只转发错误，没有普通 WAL 追加那套 `failed` 围栏 | 发布后不确定则隔离旧 writer；再次打开与对账一致。保留普通 `append_transition` 围栏 | 不选最大 `.gN` 当恢复源；不弱化普通追加；未注入 Windows 崩溃不宣称硬崩溃已测 |
| PROCESS-01 | `RecipeProofRunner` `host_trusted: true` 跳过 spawn 恢复登记；注释假定硬崩溃后无需回收。Linux 探针：父进程 SIGKILL 后独立进程组子进程可存活 | 宿主验证监督身份或 OS 生存期约束；清理确认后再复用工作区。宿主可信免除审批，不免除生存期 | 不否定已落地的协作取消桥接；Linux 探针是 OS 机制，不是 Agent 集成测试 |
| PROCESS-02 | `ProcessSupervisor::reap` 第二次有界 wait 结果忽略后无条件 `pid=0` | 未确认终态不清监督身份；返回类型化清理结果 | Unix group leader 是缓解不是证明 |
| WORKSPACE-01 | 普通 Unix confined open 无 `O_NONBLOCK`；recovery 路径已有。`project_markers` 会同步打开 | 标记探测有界；复用 recovery 打开。FIFO 名为 `Cargo.toml` 才卡住 | `.git` 等目录标记不能一律当普通文件 |
| WORKSPACE-02 | 多处先 raw HANDLE，`check_not_reparse` 失败时尚未 RAII 接管 | 立刻接管句柄；复用已有 recovery helper | 拒绝仍发生，不是路径逃逸 |
| PROVIDER-01 | Chat/Responses 非 2xx 先 `response.text().await` 再截短 | 读入阶段限额，取消/超时保持正确 | 不改模型协议权威；无 HTTP 压力测试则不写已压测 |
| PROVIDER-02 | Chat `finish_reason=length` 无独立映射 | 保留不完整终止信息；已暴露输出不透明重放 | 不无条件重试 |
| PROVIDER-03 | `framer.finish()` EOF 尾帧跳过 event 名与 JSON type 一致性检查 | 与正常 SSE 共用校验 | |
| CONTEXT-01 | `push_linked` 每实体先取桶前 64 再 newest-first；`update_entities` 用 `swap_remove` | 先按创建/新旧选择再 cap；扫描预算与候选配额分开 | 关联边不是强制正文；不宣称 GC 已误删必需上下文 |
| PACKAGE-01 | `dist.sh` / `dist.ps1` 接受 target 但不传 `--target-dir`；复用 `dist/<version>`。Bash 桩测：旧产物可被成功打包 | 下次实际发布：构建输出与复制源同一身份；干净 staging；PowerShell 原生退出码 | 不宣称当前已发布 ZIP 已错 |
| MCP-01 | 写请求阶段只有 deadline，取消在读阶段 | 仅当默认产品启用 MCP 写路径时：写/连接/读都可取消；半帧毒化 session；await reap | 不启用第二调度器；未启用则跳过 |

## 本分支已关闭，不重复立项

| 项目 | 处理 |
|---|---|
| STORAGE-01 | 已落地：Windows `MoveFileEx` 替换 metadata，不再先删；缺 metadata 且有 WAL 代际则 `RecoveryRequired`，不铸空 g1、不选最大 `.gN`。`cargo test -p agent-storage` 覆盖残留代际与覆盖写。Windows 在替换中途杀进程仍未注入 |
| DOC-01 | 活动 CURRENT 与已落地文件的矛盾已随 D0/M16-00 关闭。检查脚本仍只验结构/链接，不是全部状态断言的语义一致性 |
| 输出 EOF 后在 `select!` 外 `child.wait()` | F3-6a：`outputs_closed` 后继续守超时/取消 |
| 消费 ACK 在 `debug_assert!` 内 | F1 / `c6fbbab` |
| PromptRequired 可进入普通候选第二次 | F1 / `c6fbbab` |
| TUI resync / run_summary / shadow 去重 | F4 续审批次 |

审查重读 `12c8628` 仍会看到旧 wait/ACK 路径，不能用来否定本分支修复。

## 已并入产品切片、不是当前执行项

| 缺口 | 归属 | 状态 |
|---|---|---|
| TUI 没有 continue、忙时输入被丢弃 | M16-01 / F1 | 代码已落地；TUI 走查待做 |
| 可取消验证仅实验组合启用 | M16-03 / F3 | 取消桥接已落地；`--defer-proof` 改默认仍等慢验证走查 |
| 文件版本身份 / 清错 / grep PARTIAL / 有界读 | M16-06 / F5 | 已落地 2026-09-06 |
| Resident/Warm 对 lease/TTL 保护的处理差异 | 仅有用例触及时 | 未动；不开始通用 GC 改造 |
| 无 Cargo.toml 时空 recipe 表启动失败 | M16-08 / F6 | 已落地 |
| 无头 live 用了 permissive 审批 | M16-07 / N1 | 产品 CLI 禁止 `--yes` / `--allow-all` |
| 编辑器任务难在 argv 嵌 grant JSON | M16-07 / N2 | `--grant-file` + `--jsonl-out`；无 daemon |

## 什么可以打断主线

当前默认产品路径上已证实的权限绕过、数据破坏、重复副作用、不可恢复错误或直接阻塞本工单的崩溃。
先定位最小触发条件，修复并保留必要回归。无法可靠处理时停用受影响路径、明确限制，不假报安全，也不绕过 Core。

## 什么不自动打断主线

实验 sidecar、未启用平台能力、未出现的规模边界、性能猜想、旧窗口统计、通用化需求。
保留到历史/候选池，由实际使用或明确研究任务重新选择。MCP-01 在产品未启用 MCP 时属此类。

每条新记录只需：现象、当前 SHA、复现、影响哪个功能、处理或延期理由。
缺陷数和测试数不是交付进度；不要为每个发现新造一组里程碑。
