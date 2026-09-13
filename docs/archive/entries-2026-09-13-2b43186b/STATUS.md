# 状态入口

当前事实在 [CURRENT.md](CURRENT.md) 与 [state.json](state.json)。
当前功能队列在 [NEXT_TASKS.md](NEXT_TASKS.md)：**M17 收尾（可恢复的多入口工作台，N0–N8）**，当前工单 N0——恢复构建与验证入口（CI run `34148921895` 在 fmt 失败是直接事实；2026-09-08 切换）。顺序由 [ROADMAP.md](ROADMAP.md) 决定。
缺陷细节在 [AUDIT_TODO.md](AUDIT_TODO.md)。同一基线的审查与提案：
[2026-09-05](reviews/2026-09-05-code-review.md)、
[续审](reviews/2026-09-06-continued-audit/REVIEW.md)、
[深入续审](reviews/2026-09-06-deep-audit/REVIEW.md)、
[M16 提案](reviews/2026-09-06-m16-proposal/TRIAGE.md)、
[M17 续审](reviews/2026-09-07-platform-native-audit/REPORT.md)、
[闭环审查](reviews/2026-09-08-closure-audit-11afdd7/REPORT.md)（均为部分源码审查，不是全仓完成；提案不是已落地声明）。

本文件不再重复 M10–M15 关闭过程、失败窗口、候选选择和历史 P0/P1 队列。
旧正文由文档切换脚本保存在 `docs/archive/route-reset-12c8628/docs/STATUS.md`；历史实验制品保持原位和原内容。

里程碑、源码实现、产品默认启用、CI 和真实产品验收是不同事实。
不得从旧 green 或已发布 ZIP 推断当前工作树通过，也不得从实验开关下实现推断 TUI 已启用。
