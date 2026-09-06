# 工具生态候选入口

本轮只使用和补齐日常 Coding Agent 需要的已有内建工具。
插件 SDK、worker、多 Agent、远端编排、通用工具市场和新增平台协议不进入活动队列。
旧正文由应用脚本保存在 `docs/archive/route-reset-12c8628/docs/TOOL_ECOSYSTEM_TODO.md`，不是逐项执行授权。

模型工具表面与权限继续分离；工具不能绕开 HostToolPolicy、Core 审批、effect 或恢复边界。
现行边界见 [PLATFORM_SECURITY.md](PLATFORM_SECURITY.md) 与 [TOOL_RESULT_ENVELOPE.md](TOOL_RESULT_ENVELOPE.md)。
工具清单继续使用已有 `TOOL_INVENTORY.json` 和注册实现，不另造“统一 manifest 重构”前置项目。

仅当 F1–F6 的真实任务缺少具体能力时，才选择最小工具改进。执行入口：[NEXT_TASKS.md](NEXT_TASKS.md)。
