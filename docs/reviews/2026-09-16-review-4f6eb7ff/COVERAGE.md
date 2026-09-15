# 本轮读取范围与未完成覆盖

固定版本：`4f6eb7ff7c72a592e38363d75a2c7907d4fd7466`。

本表是本轮审查覆盖，不把之前不同版本的阅读自动算作当前全文覆盖。没有逐行读完的文件均明确标为局部；搜索片段不等于整个实现已经读完；提交差异清单不等于文件正文。

## 已读取源码

| 文件 | 读取范围/方式 | 实际重点 |
|---|---|---|
| Cargo.toml | 全文 | 20 个 workspace 成员及依赖声明 |
| context-simple/src/engine.rs | 指定范围 1000–1400、1610–2180、2620–3510；个别响应尾部截断不计未展示内容 | 引擎外围状态、continuation、卡片加载、必需材料化、GC、fetch、restore |
| context-simple/src/scope.rs | 1–290、480–820 | scope 生命周期、冷页之外的引用枚举、实际退休删除 |
| context-simple/src/gc/full/mod.rs | 1–250；另查退休调用片段 | GC 计划和 scope 退休接线；并未通读全部 full GC |
| context-simple/src/materializer.rs | 1–270、1000–1440 | 候选生成、foreground 与 required 的元数据解析 |
| context-simple/src/tests/cold_bounds.rs | 280–425 | S1 修复、固定预算零命中续查测试的可见部分 |
| context-contextcore/src/adapter.rs | 全文（1–310，300–405至文件末尾） | 全部 ContextEngine 委托方法及未转发的新搜索契约 |
| agent-context-service/src/lib.rs | 1–290 | 生产 handler 和协议会话；测试部分仅开头 |
| provider-openai/src/lib.rs | 860–1105 | Chat/Responses 请求映射和工具结果缓存断点 |
| provider-openai/src/retry.rs | 160–310、350–500、510–810 | 已知用量合并、observer、live retry 与取消 |
| provider-openai/src/prompt_cache/endpoint_shape_tests.rs | 130–245（至文件尾） | 当前普通内容和工具结果断点断言 |
| agent-compose/src/lib.rs | 510–665、1050–1320 | 生产 provider/observer 组合、部分基线测试 |
| agent-runtime/src/actor/model.rs | 180–280、790–900；另读 S2a/取消分支精确搜索片段 | 覆盖 helper、schema 拒绝、物理 ID 修复与取消 outcome |
| agent-host/tests/host_process_variant.rs | 1–220；另读实际 Command::new 片段 | 新独立进程测试入口和模型请求标记验证；未通读整条测试 |
| agent-storage/src/lib.rs | 1–310 | operation WAL 打开、代际与元数据校验；未通读全部存储实现 |
| agent-contracts/src/context.rs | 精确搜索片段 | last_search_coverage / search_external_continuation 默认行为；不计全文 |
| agent-core/src/kernel/mod.rs | 精确搜索片段 | Core 调用续查与读取覆盖状态；不计全文 |

## 文档及元数据

读取 docs/CURRENT.md 全文；S3_FIXED_BUDGET_COLD_DIR_RECEIPT.md 已展示部分；NEXT_TASKS 的 S2a/S3 相关搜索片段。核对 main 元数据（开头和收尾）、根树、从 258eb4eb 到当前的提交/文件差异清单、当前 SHA 的 CI 状态。引用回执仅用于辨认已关闭范围，不拿回执替代代码。

## workspace 覆盖边界

20 个 crate 均在当前 Cargo 清单中确认，但以下成员本轮没有独立正文通读：agent-platform-protocol、context-baselines、agent-process、agent-capability-process、agent-workspace、tool-runtime、agent-conformance、agent-replay、agent-eval、agent-tui。其余成员也大多仅覆盖上表指定文件/区间，不代表 crate 全部源码完成。

.NET SDK、GUI、所有工具实现、全部持久化/审批/效果路径、所有脚本与测试均未在本轮全文覆盖。GUI 后置是功能优先级决定，不是“代码已安全”的结论。

## 执行状态

| 检查 | 本轮状态 |
|---|---|
| GitHub 固定版本读取与主分支收尾核对 | 已执行 |
| 当前 SHA CI 元数据核对 | 已执行；35019429861，attempt 1 success |
| 远端所有 job 日志逐行核对 | 未执行 |
| 本地克隆 | 未成功；GitHub DNS 问题，下载备用路径也未成功 |
| 本地 cargo/dotnet 测试 | 未执行；没有可用工具链/checkout |
| 新发现故障注入回归 | 仅设计，未执行 |
| 真实供应商接受/缓存命中/净费用 | NOT_RUN |
| 仓库修改或推送 | 未执行 |
| 审查交付文件 | 已在本地生成 |

## 后续覆盖应优先扩展的区域

在收口本报告明确问题时，沿现有调用链补齐 context 契约与 service wire 的全文；下一轮不必再次把已核对源码从头泛读，而应补 Context 冷页对语义替代/关闭/GC 引用闭包的完整影响、完整 provider 终态结算、以及尚未通读的操作日志/工作区效果/平台重连路径。该顺序是审查建议，不是这些区域已有缺陷的断言。
