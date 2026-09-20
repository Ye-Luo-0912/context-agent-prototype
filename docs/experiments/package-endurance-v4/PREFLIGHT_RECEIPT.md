# V4 真实测试启动前回执

2026-09-20。状态：**本地准备与前置修复完成，真实供应商调用等待明确授权**。
启动第一段最多 16 轮的命令被自动审批审核拒绝，尚未创建模型段或 API 账本。
真实供应商请求 **0**，费用 **0**，候选 `app/snapshot.py` 尚未生成。

## 已准备的可复核任务

工作区位于 `target/package-endurance-v4-20260920/model/workspace`，510 个受保护文件
逐项摘要一致。既有部署应用明确保留 assisted 来源；只有快照模块和它的自测
允许模型新增。[任务规范](../../../scripts/package_endurance_v4/SPEC.md) 已冻结到
该新工作区，包含 5 条历史回执、3 个 current scope 的真实 SQLite/文件公开 fixture。

独立控制器 [verify.py](../../../scripts/package_endurance_v4/verify.py) 定义 26 个
命名核心场景：确定性导出/只读 inspect/原子 restore、完整历史、幂等、真实 exit 73、
源副文件和发布义务拒绝、损坏/缺失、旧指针、ZIP 路径穿越/重复/多余/链接/资源上限、
JSON 规范性、目标冲突及 CLI 成功/失败单文档。拒绝路径核对整个隔离 case 目录，
防止 SQLite URI 等错误在源目录外留下文件却被当作“输入未修改”。
这是有限具名 oracle，不是任意 ZIP、恶意并发文件系统替换或物理断电的证明。

没有候选模块时，公开基线因 `ModuleNotFoundError` 失败；独立 oracle 明确返回
`candidate_available=FAIL`，没有执行功能案例，不能将负例拒绝拼成候选通过。
oracle 的真实 fixture 正对照与改坏引用 blob 的负对照本地自检通过。

## 预检暴露并修复的问题

1. 通用 runner 原来无条件将实际 Chat 配置改为 Responses。现按显式配置/环境选择
   协议，relay 不转换正文；Chat 使用 prompt/completion/cache 的严格字段解析，
   缺失用量保持未知。账本、attempt 和回执携带协议身份，跨协议继承在发送前拒绝。
   两个旧实验入口显式固定 Responses，保留其历史口径。
2. 产品端没有显式关闭 Chat thinking 的入口。新增
   `OPENAI_CHAT_THINKING=provider_default|disabled`，仅固定 Chat 可显式 disabled；
   compose 真正接入 transport，profile 身份与 banner 一致。默认 wire/digest 保持
   兼容，不实现 enabled 思考历史回传，也不通过 relay 暗改请求。

依据官方文档，DeepSeek Chat 默认启用 thinking，带 tools 的多轮需要回传相应
`reasoning_content`。本次选择明确的非思考 profile，而不是假定旧 adapter 已支持
该历史契约。[供应商文档](https://api-docs.deepseek.com/guides/thinking_mode/)

## 实际本地验证

| 检查 | 结果 |
| --- | --- |
| `python -B -m unittest discover -s scripts/tests -v` | 65/65，通过 Chat 本地 HTTP fixture 红→绿 |
| `python -B -m unittest discover -s scripts/package_endurance_v4/tests -v` | 6/6，预算/未知费用/期限/oracle 反例 |
| compose 模型配置定向 | 8/0，含两轮真实 localhost HTTP 工具往返 |
| provider 定向 | 3/0，默认不变、协议限制和拒绝不降级 |
| 两 crate 全测试 | provider 172/0；compose 72 通过、7 个付费 ignored |
| 两 crate all-targets Clippy | 通过 |
| `cargo check --workspace --all-targets` | 通过，不能代替 workspace 全量测试 |
| 新 `agent-tui`、`agent-host` 构建 | 通过，摘要见状态文件 |
| fmt / 文档 / diff 检查 | 通过 |

证据：[状态](preflight-evidence/STATUS.json)、[文件清单](preflight-evidence/MANIFEST.json)。
日志包含真实 HTTP 本地红例，合成请求不计为真实 DeepSeek 请求。
本次新增未提交源码包含 `crates/provider-openai/src/chat_thinking.rs`；旧 v2/v3
冻结证据未改写。

## 尚需确认的真实调用范围

拟将隔离工作区内的合成测试规范、示例应用源码、测试数据和执行结果发送到
`api.deepseek.com`，使用已有 DeepSeek Flash 凭据；新 campaign 上限为 96 次
主决策、128 attempts、估算 USD 1、4 小时，第一段最多 16 次决策。密钥只用于
认证头，不进入发送正文或日志。详细停止条件见 [PLAN.md](PLAN.md)。

自动审批审核明确拒绝了真实启动，理由是缺少对该第三方发送范围和付费调用的
明确授权。确认请求已经发出；未收到用户答案前不重试、不转用其他通道执行。
本回执不是模型开发完成、真实流程通过或候选应用验收。
