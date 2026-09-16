# 当前阶段执行补充 — 3bdb269c

本文件是审查建议，未应用到仓库。沿用原 A/B/C 三线，不建立新的架构阶段；GUI 后置。R 编号仅定位本报告，不替代仓库既有任务号。

## 开工检查

核对工作树 HEAD、分支和未提交 diff。若不是本报告 SHA，先比较涉及文件；不得覆盖别人的在飞修改。对已合入修复只补残余，不按历史问题全文重做。

先声明每个切片的实际用户动作、最小反例和预期事件身份。不要通过给测试每个事件随机新 RunId、删除反例或扩大固定时间限制来“修绿”。

## A1 — 真实流式事件与幂等读模型（R1＋R7）

**用户动作**：同一会话能看到流式回答和重试，发生日志补齐后对话不重复、不消失；辅助内存有界。

修改入口：LiveSink 使用契约（必要时只加测试暴露），TUI event reducer、transcript window、输入身份索引。不要把实时事件写入 WAL 作为快捷修法。

验收：
- 一个 RunId，ModelStarted/delta/retry 共享生产 cursor；两段文本按顺序可见。
- stale operation/generation 仍然拒绝；重复持久事件只计一次。
- 同一日志重放两次后可见事件序列一致；同文不同事件保留。
- 400 行窗口之外的身份集合也有声明上限；当前草稿和审批不受重建破坏。

停止：真实生产者形状回归和既有 TUI 测试通过即可；不开发完整 Markdown 编辑器或另一个事件总线。

## C1 — operation 结果和费用分开结算（R2＋R3）

**用户动作**：取消任务 A 后继续 B，A 的迟到费用仍进入账目，B 的运行状态不被改写。

修改入口：actor/tools.rs stale settlement、turn.rs usage state、共享 RuntimeEvent/StatusProjection（共享契约由单一集成人负责）。

验收矩阵：
| 取消后迟到结果 | 业务行为 | 费用行为 |
|---|---|---|
| ModelOutput，含 usage/工具调用 | 不采纳正文作为当前回答；不执行过期工具 | 已知计数正式结算一次 |
| Failed，含 usage | 不重启旧操作 | 已知计数正式结算一次 |
| Cancelled，含 known_usage | 保持取消 | 现有补账能力保留 |
| 任意类型，无 usage | 保持原业务结果 | Unknown 保持 Unknown |
| 同一 completion 重复到达 | 不再执行 | 不再加账 |

在 `OPENAI_RETRY_METRICS_FILE` 不存在时跑；不能依赖可选 observer。A 的补账出现在 B 的 ModelStarted 或 ToolStarted 后时，B 的 in-flight 仍保持。确认 success/failure/maintenance 既有路径不退化。

停止：受控 provider + 真实 Runtime/事件输出通过；真实 vendor 净成本仍留在 T8，不阻塞这个本地切片。

## A2 — 任务卡归属、发布顺序与完整计数（R4＋R5）

**用户动作**：连续完成 A、B、C，重启仍看到最新正确卡片；第 33 个检查失败不能显示为零失败。

修改入口：ResultCard、发布器和 format_result_lines。总计先按唯一事件折叠，显示裁剪后做。发布序号不能随切换任务归零；重放不逐个发布历史卡片。

验收：
- A/B/C 真实写入同一 latest 路径，最终 task_id 为 C。
- 人为反转异步写入完成顺序；旧快照不覆盖新快照。
- 32 成功 + 1 失败 → 总计 33、失败 1、显示遗漏 1（或者等价明确口径）。
- 重复事件、任务切换、日志重放不重复计费/计检查。

停止：一个小型发布器或现有 gate 的正确身份即可；不要增加第二份任务数据库。

## A3 — 命令顺序与可核对的审批布局（R6＋R8）

两片在同文件有重叠时串行合并。

**控制部分**：普通文本与任务语义命令共享保序提交通道；取消/退出有明确优先级和排队项结算；session 拥有 worker 生命周期。不要所有东西都变成互不相关的 spawn，也不要把取消排在无法中断的存储尾后。

**显示部分**：布局与测量复用同一套算法/库语义，保留完整审批资料和 request_id。不要继续扩展手工 Unicode 表。

验收：暂停 worker 后输入 `/task B` + 普通文本；恢复命令 + 普通文本；窄窗口下按词折行的长参数能翻到末尾；组合字符/ZWJ/CJK 光标正确；真实 ui::render/TestBackend 验证，不只看辅助字符串。

停止：现有 TUI 可用性边界收口，不启动 GUI 或通用前端重写。

## B1/B2 — 沿原后端工单推进

源代码比较表明本轮未改变 context-simple。原 B1（批量 required 规划）和 B2（首次 card 认领验证）继续沿既有所有权执行；原 B3（ledger 导出取消安全）可后置。文档记载他人本地在飞不代表 main 已包含，先与实际工作树核对。

## T8 — 条件性的供应商 KV 成本对照

先有正确 final request、合法 wire、完整 operation 用量，再测实际供应商。场景固定同一任务/起点/验收；至少覆盖前缀稳定、动态尾部、文件版本变化、checkpoint、维护、失败重试及取消补账。

报告 uncached/read/write/output、主/维护调用、尝试数、未知覆盖及任务质量；不以 token 更短或命中率更高替代净费用结论。没有授权/凭据/预算则 `NOT_RUN`，不得自动借用环境密钥发起付费实验。

## 运行检查（本环境未执行）

先使用仓库已有隔离测试环境，确认不会访问真实 provider。以下是建议命令，不是执行回执：

```bash
git rev-parse HEAD
git status --short
cargo test --locked -p agent-tui -- --list
cargo test --locked -p agent-runtime -- --list
cargo test --locked -p agent-compose -- --list

cargo test --locked -p agent-tui
cargo test --locked -p agent-runtime
cargo test --locked -p agent-compose
cargo test --locked -p context-simple
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
```

新增定向测试使用过滤器前，核对 `--list` 中确实存在；零测试运行不算通过。共享契约合入后执行既有对应跨 crate 集成和原 CI，不新增一套平行总门禁。

## 文档收口

CURRENT 只记录当前实际能力/限制/SHA。NEXT_TASKS 只记录上述仍开放动作与停止条件。旧 U/W/T 的细节留在已有回执；不要把本报告再全文追加到当前入口。
