# 可执行任务队列

有效范围见 [CURRENT.md](CURRENT.md)。本文件只保留本阶段仍需动作的任务；历史缺陷描述、验证日志和已关闭细节只链接到原回执，不复制正文。

## 接手规则

先核对当前分支、HEAD 和未提交 diff。收尾已观察到 `27828014` 合入 A/C 相关实现，旧回执中的“未提交”只属于旧时间点。MERGED 只说明代码进入目标分支，不代表 CI 或真实供应商验收已通过；这两项分别附来源。

本轮 A=执行核心/工具，B=上下文/GC/搜索，C=平台/供应商 KV。以下编号仅在本轮有效，引用旧问题时同时写报告日期和 N 编号。每条任务一个负责人；共享契约由集成人统一合入。

## 当前工作

### 文档入口收口（集成人；文档任务）

结果：新接手者能从稳定约定、当前事实和这份队列确定一个下一动作，不再被旧 GUI/N0/M17 状态带回历史任务。

修改：AGENTS/CURRENT/NEXT/ROADMAP/STATUS，README 的过期状态段；处理 state.json 的重复状态角色，同步调整 `scripts/doc_consistency.py`。保存旧正文、维护历史相对链接，不删除冻结证据。

验收：当前任务入口唯一；准确区分 A/C 已合入的代码与尚未通过的本 SHA 验证；当前路线不要求 GUI 扩展；文档检查通过。不要在这一片顺带重构全部契约或重跑付费实验。

### B2 调用方完整性收口（B 线）——已关闭（2026-09-14）
`hydrate_all_pending_cards` 返回 `hydration_complete`；`storage_gc_protecting`/`reconcile` 以 `roots_complete && metadata_complete` 共同决定删除许可，延期原因类型化入报告；search 空命中遇未读 pending 页 fail-closed。回归 3 项红→绿（unread citation defers GC / unread page never complete-zero-match / pending owner survives reconcile）。context-simple 398/398。原记录（历史）：B2 调用方完整性收口

结果：临时无法读取的冷元数据仍是已知 owner；搜索不假装完整，GC/reconcile 不在未知引用关系下删除或重新认领它。

入口：`context-simple/src/engine.rs` 的 hydration/search/GC/reconcile 调用方及 `store.rs::plan_storage_gc`。保留已合入的 N01–N03 修复。

实施：hydration 返回完整性/剩余数量，或调用方在相同 op_gate 内显式检查 pending。未完成时先采取保守延期，不新增调度框架。搜索沿现有接口给出可重试/不完整结果；reconcile 不把 pending owner 当孤儿。只把 pending id 加进根集合并不足以保护尚未读出的出边。

回归：使用既有读卡故障注入，让 source A 留在 pending，A 强引用满足删除条件的 target B；target 没有其他独立根时也不得被删。故障清除后 A 元数据可读、B 正文可取。另验证搜索不会把未读冷页误报为完整零命中。

状态：控制流已静态确认；审查环境未执行 Rust 反例。验收不依赖真实模型。

### A1–A3 已合入实现的集成收口（A 线）——已关闭（2026-09-14）
clippy 阻塞修复入 main（`a20997b3`＋本地复核）；A2 grace 重置曾被遗留的 RED_CHECK 短路、已恢复并加固回归（`4f2fb74f`，CI run `34786157399` 七 job 全绿含 ubuntu part 1 的 unix-gated 用例）。原记录（历史）：A1–A3 已合入实现的集成收口（`27828014`，CI_FAILED）

不要重新实现 N07–N10。先核对 `27828014` 与 [实施回执](reviews/2026-09-14-backend-review-6eda2474/A_LINE_A1_A2_A3_IMPLEMENTATION.md)。当前 CI 的 Linux 日志定位到 session.rs:2156：`assert_eq!(output.metadata["signal"].is_null(), false)`，应使用 `assert!(!output.metadata["signal"].is_null())`；不要禁用 Clippy 或把它另立成新阶段。再核对真正的行为边界。

结果：session 能区分进程终止、成功/失败与输出排空；poll 有总预算且不拖住其他 session；退出后 grace 从实际退出计时；MCP 分页发现完整或明确拒绝不完整结果。

验收：实现及必要回归进入目标分支，绑定准确代码 SHA 的既有 CI 通过。Unix 限定场景须有 Linux 结果，不能用其他平台未执行冒充通过。相关 API/输出契约有变化才更新对应参考文档。

### C1–C2 已合入相关实现的 wire 验收（C 线）——已关闭（2026-09-14）
`cache_routing_wire_acceptance` 3 测试走真实 compose→runtime→provider→本地 HTTP 捕获全绿（key 形状/稳定性、B0/B1 SiblingField 映射、maintenance lane、未确认端点剥除），compose 全目标 61 绿（`44cdd6dd`）。原记录（历史）：C1–C2 wire 验收（`27828014`）

不要重新实现 N04–N06。新提交含相关 KV 接线与 cache_wire_flow 回归；先核对新增 diff、[实施回执](reviews/2026-09-14-backend-review-6eda2474/C_LINE_C1_C2_IMPLEMENTATION.md) 和当前生产调用点。

结果：稳定缓存路由键由正式组装路径填写；空稳定证据只声明策略边界；多断点在最终 wire 正确映射；无效 split 不 panic；未确认能力的端点不收到专属字段。

验收：走真实 Compose→Runtime→Provider→本地 HTTP 捕获服务器，证明连续请求的 key、断点和写策略正确；明确这只验证客户端接线，不证明供应商接受/命中/降本。附修复提交和绑定该代码 SHA 的 CI，不借用前一提交的绿色结果。主调用与维护调用分别覆盖。

## 阶段收尾（集成人）——已执行（2026-09-14）

旅程各环映射到已执行且全绿的既有回归（host 旅程双端点、B 线服务重启与分片恢复、B2 外置/恢复、process.session 真实终态、MCP 分页发现、C 线 wire 验收），不新建评测框架。[旅程回执](reviews/2026-09-14-backend-review-6eda2474/STAGE_CLOSING_JOURNEY_RECEIPT.md)。真实供应商费用对照（C3）仍为条件任务 NOT_RUN。

只有这些动作和明确风险处置完成后，才选择下一个主体功能切片。不要把已关闭报告再次整份追加回队列。

## 条件任务与后置项

C3：有明确实验授权、凭据和预算后，用固定任务做真实供应商接受/缓存读写/总费用对照。条件不满足记录 NOT_RUN，不将其设为普通编译/集成的阻塞项，也不宣称成本已验证。

后置：总历史元数据驻留分页与整体维护工作预算，按已知规模边界继续；GUI 功能、一般性排名研究和新的编排架构不自动开工。出现实测瓶颈或新的用户目标，再明确一个任务，不追加无边界研究清单。
