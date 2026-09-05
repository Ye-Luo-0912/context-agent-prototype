# Coding Agent 开发约定

当前目标：把已有 Runtime 变成日常可用的单用户、本地、单工作区 Coding Agent。
先接通功能，不继续把通用平台、研究评测和边角加固排在所有功能之前。

## 从哪里开始

默认只读 `docs/CURRENT.md` 与 `docs/NEXT_TASKS.md` 的当前任务。
`docs/ROADMAP.md` 决定交付顺序；`docs/AUDIT_TODO.md` 只做缺陷分流，不是全仓清零前置门。
实现时再读相关模块、调用方、现有测试和对应契约文档。
历史报告、`docs/archive/`、旧 M15 窗口和旧 TODO 不生成新任务。

## 必须保留

- RuntimeActor 是唯一编排者；沿用 TaskAnchor、ExecutionState、Checkpoint 和 RuntimeHandle。
- Core 继续负责审批、权限、effect 身份、提交与恢复；不能为减少回合绕过它们。
- ContextEngine 可替换；工作集与原始历史分开，工具结果与正文保持有界；工具不私读 ContextEngine/记忆存储。
- 保持现有 crate 依赖方向和 conformance；ToolSpec/模型生成字段不是权限，外部正文不提升为 system 指令。
- GC 的可逆外置不等于删除；只有既有 Storage GC 按保留与引用规则删除，语义终态不能被热度或 lease 复活。
- 不覆盖用户已有修改；不假报验证、完成、恢复、CI 或实验结果。
- 计划勾选不是可信验证；恢复记录不是重放副作用的授权。
- 不新增 trace 数据库、通用 Planner、TaskGraph、并行 worker 或第二套状态权威。
- 默认保持现有 GC/打分策略。修复已确认错误不等于开启算法研究。

## 交付规则

一次只交付一个功能切片。先写清用户能做什么，再改相关代码，补能防止该功能回归的必要测试。
文档任务只跑现有文档检查；功能开发先做定向检查，合并沿用现有 CI，不另起全仓反复验收循环。
真实的权限绕过、数据破坏、重复副作用和当前路径崩溃必须处理；无关边角问题记入 backlog，不接管主线。
冻结实验只在明确选择研究任务时重跑，不为普通产品改动重新开启 M15/LT-EVAL。
任务达到验收即停止扩展，报告实际执行命令、结果、限制和下一任务。
除明确要求外，不新建 release/tag，不改冻结证据，不只提交测试和文档冒充功能完成。
