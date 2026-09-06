# M16 任务文档包

**活动工单是仓库根下的 [`docs/NEXT_TASKS.md`](../../NEXT_TASKS.md)。** 本目录只保存提案原文，不进默认必读。

本包是 `context-agent-prototype` 下一大阶段的**提案和可执行任务队列**，不是源码归档，也不是实现补丁。

- **M16_PLAN.md**：阶段范围、架构、19个crate影响地图、核心资产路线、退出条件。
- **PROPOSAL_NEXT_TASKS.md**：针对 `12c8628` 的替换稿原文（全部标待实施）。
- **TASKS.json**：同一任务数据的机器表示；不是需接入产品的新系统。
- **CODE_BASIS.md**：实际阅读与未覆盖范围。
- **TRIAGE.md**：本工作树对照。

基线：`12c86283b8d5991e9f17a07f14871dcf39d65066`。本轮clone失败，完整代码审查与Rust测试未完成；所有工单均未在远端实施。

## 给Coding Agent的启动指令

> 执行本包NEXT_TASKS。先核对当前HEAD和用户未提交修改，保留一切用户改动；当前代码已做完的切片定向确认后跳过。完成M16-00的最小文档切换后立即做M16-01，接/continue、忙时补充反馈和可信预检，修已定位的release ACK及重复装配问题，跑对应回归并给出一个可演示结果。再沿唯一队列推进。不要把Chronicle、TaskGraph、数据库、worker、向量或新全量评测框架当作前置。权限、持久性和真实恢复不降低；未跑不写通过。

## 本地盘点（可选；不是新的CI门禁）

```bash
python /path/to/agent-next-stage-20260906/inventory_checkout.py \
  /path/to/context-agent-prototype \
  --out /path/outside/repository/source-inventory.json
```

输出记录实际HEAD、Git文件类型/对象身份、工作树状态、子模块及缺失文件。每个源文件的review状态固定初始化为`NOT_REVIEWED`。脚本不联网、不checkout、不build、不执行仓库代码、不提交、不清理文件。

只有NEXT_TASKS需要作为活动工单合入仓库；其余可留在审查工件中。不自动覆盖AGENTS/安全契约/历史evidence，不增加一批新的默认必读文档。
