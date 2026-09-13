# 读取范围与验证覆盖 · 6eda2474

## 如何解读

本清单记录本轮静态续审的读取范围，不是“已审查且无缺陷”的认证。所有链接固定到 `6eda247401092a18bcbd547e475bc79b1ed3a4c1`，不随 main 移动。标为“范围”的文件在其余行仍可能存在问题；标为入口/导出的文件不能代表其模块实现已通读。目录、搜索片段、源码范围、实际运行测试是不同证据等级。

本轮核对了20个 crate 的 workspace/目录组成，每个 crate 有以下至少一处直接代码或入口检查；并未逐行读取全部 crate、测试、SDK、GUI 和脚本。没有执行本地 Rust/.NET 或付费供应商实验。远端 CI 成功单列，不计入人工源码阅读覆盖。

## 当前源码窗口

| Crate | 文件 | 本轮读取范围 | 覆盖限制 |
|---|---|---|---|
| agent-capability-process | [crates/agent-capability-process/src/mcp.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-capability-process/src/mcp.rs) | 1–330；520–850 | 范围审阅：发现、调用、连接和 manifest 建立；中段与后部并未逐行读完 |
| agent-compose | [crates/agent-compose/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-compose/src/lib.rs) | 1–310 | 范围审阅：引擎构建、预算注入、供应商配置身份；其余组合代码与 tests 未全读 |
| agent-conformance | [crates/agent-conformance/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-conformance/src/lib.rs) | 入口文件 | 仅导出与契约说明；checks.rs / report.rs 未逐行审查 |
| agent-context-service | [crates/agent-context-service/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-context-service/src/main.rs) | 入口文件全文 | 已审阅启动到 serve 的生产入口 |
| agent-context-service | [crates/agent-context-service/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-context-service/src/lib.rs) | 1–155 | 范围审阅：build_engine、ServiceOp 分发；帧循环和其余测试未全读 |
| agent-contracts | [crates/agent-contracts/src/model.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-contracts/src/model.rs) | 1–335；398–630 | 范围审阅：TurnFrame、wire、ModelInput、split；其余模型契约及测试未全读 |
| agent-contracts | [crates/agent-contracts/src/model_cache.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-contracts/src/model_cache.rs) | 文件主体 | 复用 hint 验证路径；子模块 tests 未全读 |
| agent-core | [crates/agent-core/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-core/src/lib.rs) | 入口/导出 | 不能把导出列表视为全部内核实现已审查 |
| agent-core | [crates/agent-core/src/approval.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-core/src/approval.rs) | 1–345 | 范围审阅：审批生命周期；后部未全读 |
| agent-core | [crates/agent-core/src/kernel/mod.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-core/src/kernel/mod.rs) | 1–240 | 范围审阅：内核入口；子模块未全部逐行读完 |
| agent-eval | [crates/agent-eval/src/metrics.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-eval/src/metrics.rs) | 1–280 | 主要读到计量结构定义；不是全部聚合器/分析算法审查 |
| agent-host | [crates/agent-host/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-host/src/lib.rs) | 300–650 | 范围审阅：正式宿主路由；其余 transport/测试未全读 |
| agent-platform-protocol | [crates/agent-platform-protocol/src/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-platform-protocol/src/work.rs) | 1–275 | 路由和边界契约；验证器与其他协议文件未全读 |
| agent-process | [crates/agent-process/src/watchdog.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-process/src/watchdog.rs) | 1–365（返回有截断） | watchdog 主路径与重试；不认定所有测试行已读取 |
| agent-replay | [crates/agent-replay/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-replay/src/main.rs) | 1–260 | 回放入口/参数与模式 |
| agent-replay | [crates/agent-replay/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-replay/src/lib.rs) | 1–245 | 导出、配置、输入工件有界读取；实际事件重放主循环未全读 |
| agent-runtime | [crates/agent-runtime/src/prompt.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-runtime/src/prompt.rs) | 315–520；1330–1460 | 组装、分段及状态投影；其他范围/全部测试未全读 |
| agent-runtime | [crates/agent-runtime/src/instance.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-runtime/src/instance.rs) | 1–335 | 检查点平面与实例入口；不等于所有恢复参与者已全读 |
| agent-runtime | [crates/agent-runtime/src/platform/work.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-runtime/src/platform/work.rs) | 650–970 | 正式检查点/恢复控制路径 |
| agent-runtime | [crates/agent-runtime/src/actor/model.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-runtime/src/actor/model.rs) | 本轮生产调用的搜索片段 | 确认 into_request 直接交 complete_stream；本轮未全文件重读，旧轮范围不算本轮覆盖 |
| agent-storage | [crates/agent-storage/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-storage/src/lib.rs) | 1–270 | 文件事件存储入口；其余 journal/checkpoint 实现未全读 |
| agent-tui | [crates/agent-tui/src/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-tui/src/main.rs) | 1–245 | 启动、授权、组合和恢复入口；UI及会话子模块未全读 |
| agent-workspace | [crates/agent-workspace/src/broker.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/agent-workspace/src/broker.rs) | 1–315 | 输出 broker 生产主体与部分回归；其他 workspace 文件仅目录核对 |
| context-baselines | [crates/context-baselines/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-baselines/src/lib.rs) | 1–360 | 导出与部分实验回归 |
| context-baselines | [crates/context-baselines/src/rolling.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-baselines/src/rolling.rs) | 1–410（一次返回截断）；345–615；615–文件尾 | 覆盖主要 Rolling 生产路径；不声称其他 baseline 文件全读 |
| context-contextcore | [crates/context-contextcore/src/adapter.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-contextcore/src/adapter.rs) | 1–315 | 服务适配与保护根操作；后续/协议子模块未全读 |
| context-simple | [crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/engine.rs) | 700–1175；1430–1705；1740–1945；2090–2285；2420–2675 | 多段审阅：hydration、ingest、GC、reconcile、search、restore、storage GC；存在未读间隔 |
| context-simple | [crates/context-simple/src/store.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/context-simple/src/store.rs) | 1–350；1150–1765 | card/blob、存储 GC、reconcile；中间检索/召回段未全读 |
| provider-openai | [crates/provider-openai/src/prompt_cache.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/provider-openai/src/prompt_cache.rs) | 文件主体 | 模式配置与兼容判断；tests 未本轮全读 |
| provider-openai | [crates/provider-openai/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/provider-openai/src/lib.rs) | 850–1125 | 两种 wire mapper及缓存映射；流解析/传输主体未本轮全读 |
| provider-openai | [crates/provider-openai/src/retry.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/provider-openai/src/retry.rs) | 213–420；625–845 | 已知尝试聚合和主要重试出口；其他范围未全读 |
| tool-runtime | [crates/tool-runtime/src/tools/session.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/session.rs) | 1–145；567–686 | drain/poll；start/stop 其他范围未本轮全读 |
| tool-runtime | [crates/tool-runtime/src/tools/process.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/process.rs) | 1–380；970–1240 | 进程配置/执行尾与监督接线；有未读间隔 |
| tool-runtime | [crates/tool-runtime/src/tools/shell.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/shell.rs) | 235–755 | 执行主循环、结果与部分测试；前部 dialect 未本轮全读 |
| tool-runtime | [crates/tool-runtime/src/tools/search.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/6eda247401092a18bcbd547e475bc79b1ed3a4c1/crates/tool-runtime/src/tools/search.rs) | 370–430 | 本轮确认 scan_continuation 已进入 spec；不是全部搜索实现重审 |

## 根目录与外围

| 范围 | 本轮状态 |
|---|---|
| 根目录、crates 成员、部分 crate 的递归树 | 已核对目录；不等于所有文件内容已审查 |
| docs/CURRENT.md 与 KV 回执 | 读取开头/相关片段；大返回有截断；未审完全部历史 review 文档 |
| 本次提交与489对照 diff | 已请求，但大结果有截断；不宣称全部 diff 已读完 |
| clients/dotnet | 本轮未按当前 SHA 全量重读；先前轮次内容不算本轮核实 |
| apps/Agent.Desktop | 本轮未重新逐行审查；GUI 功能仍暂缓，不等于认证现有 GUI 无缺陷 |
| scripts、examples、.github 配置、辅助 Python/Shell | 根目录/部分元信息核对；没有全部正文审阅 |
| tests、内嵌 #[cfg(test)]、夹具与快照 | 只读取源码范围中部分测试；远端 CI 通过不是人工逐条审阅 |
| 外部依赖/生成内容/历史运行仓库副本 | 未全量审计；历史副本不能代替当前生产文件 |

## 尚不能声称的事情

不能声称“所有代码已读完”“全部路径安全”“新增反例已本地复现”“真实 KV 命中率提高”“任务成本已下降”。没有可靠的全树内容与行覆盖分母，因此不提供看似精确的全仓阅读百分比。

## 验证记录

- 固定代码 SHA：6eda247401092a18bcbd547e475bc79b1ed3a4c1。
- 固定 SHA 的 CI run34766940848：completed/success；现有测试的远端证据。
- 本地命令能力：Git 在；cargo/dotnet 未找到；GitHub DNS 解析失败。
- 本轮 Rust/.NET 测试：NOT_RUN。
- 本轮真实供应商调用：NOT_RUN。
- 新增回归：仅定义待实施场景，未创建到仓库、未执行。
- 仓库写操作：NONE。只在工作容器中创建了审查交付物。
