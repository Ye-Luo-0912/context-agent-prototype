# 当前事实与工作范围

本页是当前状态入口，不是历史流水账。执行任务及状态只维护在 [NEXT_TASKS.md](NEXT_TASKS.md)。旧报告中的“当前”只属于其固定基线。

## 已核对基线

文档主审查基线：`2b43186b005b5e86172037f98206f4417c7e2bca`；收尾新增提交核对：`278280146df5934828962253507553762693f494`（东京时间 2026-09-14 06:19:53）。采用本文前仍须核对实际分支与工作树。

已知 CI：前一 SHA `2b43186b` 的 run `34777279921` 成功；新 SHA `27828014` 的 run `34783505990` 首次运行失败，Linux/Windows 在 Clippy 步骤失败，后续 Rust 测试矩阵被跳过。Linux 日志定位到 `tool-runtime/src/tools/session.rs:2156` 的布尔 assert_eq 写法。修复和重验属于小型集成收口；前一提交的 green 不外推到本提交。

新提交已包含 A1–A3 与 C 线 KV 接线相关改动，不再按“仅本地回执、代码尚未提交”派工。下一动作是核对新增实现/必要边界、修复当前 CI 阻塞并完成原验收，而不是重新生成 N04–N10 实现。新提交完整 diff 尚未逐行审阅，不能从提交说明推出所有行为已获独立确认。

## 当前范围

优先完成可持续执行、可纠正、可取消、可冷恢复、结果可核验的后端长流程。核心、上下文、GC、搜索、工具、最小平台与供应商 KV/Prompt Cache 同属主体。GUI 只维持必要兼容与正确性修复，不扩展产品功能。

工作线固定为：A 执行核心与工具；B 上下文/GC/搜索；C 平台/供应商缓存与成本。旧报告的 A/B/C 字母仅是历史别名，不能单凭字母推断本轮所有权。

## 已落地主体与限制

B1–B3（本轮 N01–N03）修复已进入 main：保护性卡片清理、pending owner 取消安全、卡片有界及哈希/结构校验。依据：[B 线回执](reviews/2026-09-14-backend-review-6eda2474/B_LINE_RECEIPT.md)。这些已修行为不原样重开。

A1–A3 与 C1–C2 原回执记录的是本地实现；其相关代码现已随 `27828014` 进入 main，但本 SHA 的 CI/端到端验收未关闭。原记录：[A 线](reviews/2026-09-14-backend-review-6eda2474/A_LINE_A1_A2_A3_IMPLEMENTATION.md)、[C 线](reviews/2026-09-14-backend-review-6eda2474/C_LINE_C1_C2_IMPLEMENTATION.md)。下一动作和验收归 NEXT_TASKS。

本次续审发现 B2 的调用方残余：`hydrate_all_pending_cards` 可因暂时 I/O 失败而提前返回，部分 owner 仍 pending；Storage GC/reconcile 等调用方没有因此降低完整性判定。控制流已静态核对，故障注入回归尚未在审查环境执行。它是既有修复的接缝，不是否定全部 B 线成果。

正式 `agent-host` 未指定策略时仍默认 Rolling；Dynamic 是可选实现。不能用 `state.json` 中旧的 dynamic 默认记录替代实际入口配置。配置依据放在 [CONFIGURATION.md](CONFIGURATION.md)，更改默认值属于单独产品决定。

尚不能宣称：无限历史热内存有界、全部源码逐行审查完成、A/C 新实现已获本 SHA 的完整绿色 CI 验证、供应商 KV 已实测降低任务费用。真实模型实验按预算和凭据条件执行，不阻塞无须模型的生产接线。

## 按需阅读

架构边界：[ARCHITECTURE.md](ARCHITECTURE.md)；上下文规则：[CONTEXT_LIFECYCLE.md](CONTEXT_LIFECYCLE.md)；恢复操作：[RECOVERY_RUNBOOK.md](RECOVERY_RUNBOOK.md)。只读当前任务相关部分。

历史报告及冻结证据保留原位置。旧 `state.json` 的里程碑数据不参与当前派工；迁移时应撤销其重复“当前状态”角色，并同步调整文档检查脚本。
