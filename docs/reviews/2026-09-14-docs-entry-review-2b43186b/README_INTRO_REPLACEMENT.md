# README 定点替换说明

仅替换根 README 开头从 “The concrete product target …” 到 Architecture 之前的旧阶段/CI 段落。保留假设、运行示例、配置说明和其他已有文档；采用前重新核对未提交修改。下面是英文正文，以保持根 README 原语言。

---

The product target is a single-user local coding-agent backend: one workspace,
one runtime orchestrator, explicit provider configuration, bounded tools,
authorized effects, verifiable outcomes, and checkpoint-based recovery.

The current work prioritizes the backend long-flow path: execution and tools,
context/GC/search, and platform integration with provider-side prompt caching.
Existing interfaces remain clients of the same runtime; new GUI features are
not a prerequisite for backend completion.

See [`docs/CURRENT.md`](docs/CURRENT.md) for the source-scoped implementation
status and [`docs/NEXT_TASKS.md`](docs/NEXT_TASKS.md) for executable work.
[`docs/ROADMAP.md`](docs/ROADMAP.md#route-to-a-usable-local-agent) defines capability
milestones. This README does not duplicate changing CI results or task queues.

Historical reports and experiment artifacts remain evidence for their named
source and environment; they do not certify newer code or uncommitted work.

---

另外单独修正 crate 说明：`context-baselines` 的 Rolling 不能再笼统描述为只用 fixed marker；区分无 compactor 的 baseline 形态与产品组合实际注入的压缩器。应以本次选择入口的 build_context_engine 为准，不从 README 推断默认策略。
