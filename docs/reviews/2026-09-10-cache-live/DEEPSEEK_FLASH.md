# DeepSeek Flash：状态变化后的真实缓存复用

2026-09-10，基线 `732cf93` 加既有工作树。用户指定 DeepSeek Flash 并授权使用本次提供的凭据；凭据只进入测试进程，未写入源码、配置或报告。

**本轮合成场景验收通过：当前状态变化时，CurrentStateLast 三次均读取 6,144 / 6,468 tokens（94.99%），Legacy 三次均为 256 / 6,468（3.96%）；全部 10 次回答正确。**这是共同布局在真实供应商上的有界复用证据，不是整仓任务质量或实际账单验收。

## 接入与控制条件

- 直接使用 `https://api.deepseek.com/responses`、`deepseek-flash`。官方当前推荐该名称；认证后的模型列表 HTTP 200 且包含该标识，10 次响应均自报 `deepseek-flash`。没有调用 Pro，也没有把旧网关模型别名当成本次服务身份。[模型说明](https://api-docs.deepseek.com/quick_start/pricing/)
- 显式固定 `reasoning.effort=none`，将合成查表任务的输出控制在 512 token 上限内；DeepSeek 将该值定义为关闭思考。`provider_default` 缓存，没有发送 OpenAI 专属断点、缓存路由 key 或额外预热调用。[Responses 参数](https://api-docs.deepseek.com/api/create-response/)
- 复用生产 PromptAssembler、ModelInput 共同边界及 OpenAiProvider 实际发送路径。两臂的正文、角色、工具定义、查询序列相同，只有已有布局选择和隔离用 nonce 不同；Focus/TaskProgress 持续变化。没有仓库工具执行或私人仓库正文。
- 每臂顺序：cold round 0 → change round 1 → change round 2 → change round 3 → 原样重放 round 3。两臂交替 AB/BA，首次 policy 前部各有稳定的新 nonce；请求之间间隔 5 秒。共 10 次、每次 55 秒超时与 1 MiB 响应上限，无自动重试。
- DeepSeek 文档说明默认缓存可在请求边界、公共前缀检测及固定 token 间隔持久化，且为 best effort。因此验收包含连续不同后缀，不能只靠原样重放判断收益；本次不猜测具体由哪条内部规则产生命中。[缓存规则](https://api-docs.deepseek.com/guides/kv_cache/)

## 结果

| 阶段 | 每臂调用数 | 每次输入 | Legacy 每次缓存读取 | CurrentStateLast 每次缓存读取 |
|---|---:|---:|---:|---:|
| 冷请求 | 1 | 6,468 | 0 | 0 |
| 当前状态变化 | 3 | 6,468 | 256（3.96%） | 6,144（94.99%） |
| 原样重放 | 1 | 6,468 | 6,272（96.97%） | 6,272（96.97%） |

两臂各输入 **32,340**、输出 **126** tokens。全序列缓存读取：Legacy **7,040（21.77%）**，CurrentStateLast **24,704（76.39%）**。三次变化请求的输出数量也逐次相同；答案均准确匹配当前 key、serial 和 KEEP_FOCUS，工具调用为零。

每次恰有 1 条发送观测，诊断没有丢弃，HTTP 错误为零。状态变化时，新布局保持前 4 个完整输入项、20,560 字节，按块计算的 HTTP 前缀下界为 20,480 字节；Legacy 保持前 2 项、421 字节。工具与顶层设置摘要均未变化。原样重放双方均命中，变化请求则明显分开，不能再把完整重放命中当作本轮的主要验收证据。

测试耗时 **54.78 秒**，其中包含 45 秒有意间隔。样本小、只有一个账户与一组序列，不做延迟显著性或普遍命中保证。

## 费用口径

实际账户账单未读取。按本次 09:14 UTC（周四峰时）与查询到的官方 Flash 公示单价估算：每百万 miss / hit / output 分别为 $0.3 / $0.006 / $1.2。公式为 `(input-hit)×miss价 + hit×hit价 + output×output价`。

- 三次状态变化的输入费用估算：Legacy **$0.0055954**，新布局 **$0.0004022**，下降约 **92.8%**。
- 含冷请求和重放的每臂五次总费用估算：Legacy **$0.0077834**，新布局 **$0.0025902**，下降约 **66.7%**。

这些是公示价格乘以服务端自报 usage 的估算，不是实付费用，也不外推到旧网关、其他费率或真实长任务。[公示费率与峰谷时间](https://api-docs.deepseek.com/quick_start/pricing/)

## 代码交付与验证

新增严格、类型化的 `ResponsesReasoningEffort` 与 `OpenAiProvider::with_responses_reasoning_effort`。组合入口解析 `OPENAI_RESPONSES_REASONING_EFFORT=provider_default|none|low|high|max`；非默认值要求固定 Responses 协议，错误值在创建 Runtime 前拒绝。所选档位进入 provider profile digest 与启动信息；默认不发字段并保持原 digest。请求正文、角色、权限、工具调度与选择/GC 没有因此改变。

实际本地检查：

```text
cargo test -p provider-openai --lib --quiet
117 passed

cargo test -p agent-compose --lib --test kv_cache_walk --quiet
31 passed; probe 5 passed / 5 ignored

cargo clippy -p provider-openai -p agent-compose --all-targets -- -D warnings
exit 0
```

回归覆盖默认请求不增加字段、非法值与 Auto/Chat 组合拒绝、显式档位改变配置身份、实际 HTTP 字节携带 `reasoning.effort=none` 且仍保留最新状态。没有扩大到全仓测试或新 CI。

真实执行由 [run_deepseek.py](run_deepseek.py) 启动：

```text
cargo test -p agent-compose --test kv_cache_walk automatic::compare_deepseek_flash_automatic_cache -- --ignored --exact --nocapture --test-threads=1
1 passed; 10 actual requests; 10/10 correct
```

首个启动尝试因 runpy 相对报告路径，在创建报告时本地失败，尚未调用 provider；随后修正为绝对路径。本轮实际生成调用仍为上面的 10 次，另有 1 次只读模型列表请求。

- [逐请求数值与摘要](live-deepseek-1789031645489837200.json)
- [16 个相关源码文件的副本与摘要](live-deepseek-1789031645489837200/manifest.json)，运行后逐项确认未变

本片完成。共同布局的“改当前状态仍复用稳定证据”已在 **Flash 非思考模式的合成请求**中得到验证；此前网关的显式断点拒绝结论仍保留。真实代码任务、思考模式工具续跑、长期缓存稳定性和实际账单仍未验收。下一功能切片回到现有 W04 非 BeforeModel 维护取消队列，实施前核对当前代码，不重复本组成功实测或重开冻结实验。
