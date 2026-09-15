# 4f6eb7ff 后端续审：冷目录跨层语义、供应商 KV 与维护性

## 基线、证据与范围

- 仓库：`Ye-Luo-0912/context-agent-prototype`。
- 固定提交：`4f6eb7ff7c72a592e38363d75a2c7907d4fd7466`；收尾再次读取 main，仍为此提交。
- 提交时间：2026-09-15 20:24:20 UTC / 2026-09-16 05:24:20 JST。
- 此 SHA 的 CI：run `35019429861`，第 1 次尝试，success。上一轮 `258eb4eb` 的 cancelled 状态不沿用。
- 方法：连接器读取固定版本源码、提交差异、CI 元数据，并核对官方供应商文档。本地 GitHub DNS 解析失败，未成功克隆；环境没有可用 Cargo / .NET，未执行仓库测试或真实供应商调用。
- 本轮没有完成全部文件逐行审查。目录盘点、源码片段读取、静态推导、远端 CI 和实际执行是不同证据等级；逐文件范围见 [COVERAGE.md](COVERAGE.md)。
- 本报告没有修改或推送仓库。新增回归均是建议，不能当成已经执行的测试。

## 结论

主架构继续沿用，不重写、不恢复 GUI 主线。当前应优先把“冷页仍是逻辑所有者”的规则贯穿 scope 退休、必需正文材料化和服务边界；再修续查状态的资源与失效边界、供应商工具结果断点类型，以及取消时已知用量的正式结算路径。

分页减少的是常驻元数据，不应改变记录是否存在、是否受保护、是否必需、是否允许投影等语义。当前代码已有第五种实际存在位置（只有卡片定位信息、元数据未加载），但一些旧路径仍只枚举四种已加载位置。

## 旧项核对

- S1：`cold_bounds` 自锁已改为锁内检查/复制、锁外 fetch、重新取锁验证；owner 数量已改为集合守恒。不是本轮待重做项。
- S2a：被裁剪物理 ID 的清理已有新分支，不再沿用旧“只有追加 miss 才清 ID”的描述。
- S2b：普通消息字符串已经改为 `input_text` 内容块承载断点。下文 V6 是工具结果分支的另一项类型错误，不是原普通文本问题原样重开。
- S3：预算化读取、覆盖观测、续查 token、按 ID 安装后的驻留结算均已有实现。下文检查的是这些新能力与旧调用方之间的契约。
- S4：新增测试从 Cargo 的真实 `agent-host` 二进制启动进程，并检查实际请求中的纠正标记；不再沿用“只有同进程重新 compose”的旧结论。本轮只阅读其部分源码，不声称重新执行了该测试。
- 重试失败用量并入成功/最终失败已有实现。V7 专指取消出口依赖可选诊断文件的问题。

## 发现总表

| 编号 | 建议级别 | 当前问题 | 主责任 |
|---|---|---|---|
| V1 | P1 | scope 退休遗漏未加载冷页引用，后续读回卡片可能失去 owner | Context/GC |
| V2 | P2 | PromptRequired 解析不覆盖 pending 卡片，存在正文被判 Missing | Context/材料化 |
| V3 | P2 | 服务适配器丢掉 coverage，并忽略续查 token | Context 契约/服务/Core |
| V4 | P2 | 相同普通搜索重复追加 covered_ids，固定历史也可持续增长 | 搜索/资源 |
| V5 | P2 | 恢复不清理外置续查状态，token 缺少视图版本且会重用编号 | 搜索/恢复 |
| V6 | P2 | 工具结果断点生成 output_text，不属于官方支持的输入块 | Provider/KV |
| V7 | P2 | 重试等待被取消时，已知用量只能进入可选 observer | Provider/Runtime 成本 |

P1 表示应先处理的数据可达性/恢复保证风险，不表示本轮已观察到用户数据损失。P2 表示明确契约或功能缺口；触发边界见各节。

## V1 — 未加载冷页的 scope 引用被退休扫描漏掉

### 位置与控制流

- `crates/context-simple/src/engine.rs::gc`：执行预算化 hydration，忽略返回的 `_hydration`，继续 `full::plan_full_gc`。
- `crates/context-simple/src/gc/full/mod.rs::plan_full_gc`：正常路径及空热集合路径均可能运行 scope 退休。
- `crates/context-simple/src/scope.rs::retire_closed_scopes`：referenced 集合来自 heap、Warm、pending_externalize_retry、已加载 external、active scope；没有未加载卡片中的 scope 引用。
- `crates/context-simple/src/engine.rs::hydrate_card_for`：读到卡片但其 `scope_id` 不在 scope tree 时，删除 pending 定位行，累计 missing 并返回 false。

### 条件反例

一张合法、可读、仍受当前运行持有的卡片引用已关闭 scope S。它因热上限或读取故障留在 pending。scope 数超过退休触发阈值，且没有其他已加载条目引用 S。GC 的退休扫描认为 S 无引用，移除 S；稍后按 ID 读卡片时，加载器因 S 不存在而消费定位行。后续 checkpoint 可能不再保留这条定位。

这会破坏当前运行的可达性/恢复目录。不能扩大成所有 blob 立即被物理删除；旧 checkpoint 或磁盘原始文件可能仍存在。

### 最小修复

短期把元数据/引用覆盖完整性传入 **scope 退休许可**：未证明闭包完整时不退休未知冷页可能引用的节点，其余不依赖完整信息的内存 GC 可以继续。不能通过删掉 scope 校验让坏状态进入运行。

长期沿现有目录维护可靠的 scope 引用计数或冷页引用索引，避免只有把历史全部载入内存才能退休。新索引必须与分页 owner、迁移和 checkpoint 原子保持一致，不能成为独立的第二份任务真相。

### 回归与停止条件

固定配置：创建合法冷卡片 → 留在 pending → 超过 scope 退休阈值 → GC → 按 ID fetch → checkpoint/restore。检查真实正文、scope 关系与 owner 集合保持；释放最后一个真实引用后才允许退休。测试必须证明目标卡片本轮未加载，不能因 fixture 顺序巧合进入热表而绕过反例。

停止：上述保证成立，不要求先实现无限历史或重写整个 GC。

## V2 — 必需正文规划没有解析 pending 冷卡片

`materialize` 直接在当前 State 上调用 `plan_foreground` 和 `plan_required`。后者按 ID/URI 查询 heap、Warm、写入重试表和已加载 external，实体查询也只用相同已加载索引；不命中就记录 Missing。pending_external_cards 不在解析路径中。

因此，“正文仍存在且可按 ID fetch”与“PromptRequired 能被材料化”可能不一致。泛化的前置维护可能碰巧加载目标，但在热上限或读取预算耗尽时没有保证。

最小修复：在计划必需正文前，针对有界 required refs 做目标元数据解析。按 ID 可以直接定位卡片；路径/实体依赖可靠冷目录查询。区分真正不存在、无法读取、策略排除、因预算暂未解析。不能把未装载等同 Missing，也不能强制全历史 hydration。

回归：目标位于 restore 首批之后，固定热预算，给出精确 PromptRequired URI，直接 materialize。可用正文应被提供；确实无法满足时必须给出与实际原因一致的状态。补充按路径 foreground 及服务模式等价测试。

## V3 — 覆盖状态和续查在服务边界丢失

`ContextServiceAdapter::search_external` 通过 `ServiceOp::SearchExternal` 只返回 `Vec<ExternalizedContext>`；服务 handler 也只序列化 entries。适配器没有覆盖新加的 `last_search_coverage` 与 `search_external_continuation`，于是继承默认 complete 和忽略 token 的行为。

这对始终拥有完整目录的 baseline 可以成立，对背后实际运行分页 Dynamic 的适配器不成立。非空 partial hits 到达 Core 后会丢掉未读覆盖事实；续查请求也没有真正转发到服务端的续查实现。

建议让一次搜索原子返回 hits、coverage、observation、continuation/视图身份，而不是返回命中后再读一个可变的“上一次搜索”旁路。已有类型能复用的就复用；通过既有协议版本/能力协商迁移。旧服务不能证明 coverage 时，应明确 Unknown/Unsupported，而不是默认完整。

回归：对同一冷热 fixture，比较 in-process 与 service 的非空不完整结果、空不完整结果、续查到最后一页、过期 token。hits 与 coverage 必须属于同一次查询。

这项在回执中被承认尚未接 wire，但已知限制不改变其用户可见影响；它应作为主体闭环任务，而不是无期限待办。

## V4 — 搜索 continuation 的已覆盖列表没有请求级边界

`record_search_coverage` 先取得当前 carded hot IDs，再在 query_key 相同时直接追加旧 `issued.covered_ids`；没有去重，也不要求本次请求真的持有 continuation。

在固定历史、相同热窗口、仍有未读页的情况下，重复普通搜索即可不断追加同一批 ID。一个 slot 不能证明 slot 内的 Vec 有界。复制该 Vec、遍历 pending 时进行线性 contains，还会增加查询工作量。

最小修复：明确 fresh search 与 resume search；fresh 不继承旧 walk。有效 resume 使用去重结构并设定链级条数/字节边界；超过边界返回显式状态。下一步可改为绑定目录快照的页位置/范围游标，不能用无限收集全历史 ID 的方式兑现“低内存分页”。

回归：固定 hot/pending 集合，连续重复普通查询，continuation 内存不得随调用次数增长；再测试有效续查每次推进且不漏页。不要通过减少测试循环或只限制 token 字符串长度来修。

## V5 — 原位恢复后旧续查状态仍然有效，编号存在 ABA

`search_continuation` 与 `last_search_coverage` 是引擎上、State 之外的状态。`restore` 只替换 State 并修复相关索引，没有清理这两个字段。`rotate_search_window` 只比较 token 字符串与 query_key，没有绑定恢复代际、目录版本或卡片版本。

此外，编号来自当前 slot 的字符串尾数；遍历完整时 slot 清空，下次链重新从 cold-window-1 开始。相同查询的旧 token 因此可能在新的 walk 中重新匹配。

“不写进 checkpoint”只代表新进程没有读入它，不代表对同一个引擎调用 restore 时它会自动消失。

最小修复：成功 restore 使旧 token 明确失效；失败 restore 不破坏现有运行的合法状态。使用进程生命周期单调 nonce 或随机不透明 identity，并绑定目录/恢复代际。目录变化后的处理必须明确，不允许旧 covered IDs 作为新视图的覆盖证据。

回归：在 V1 视图签发 token；恢复 V0（同 ID 可拥有不同卡片 hash/元数据）；使用旧 token，不得跳过 V0 中尚未搜索的内容。补 complete 后新链编号重复、无 token 的 fresh query、拒绝 restore 不改变既有链等场景。重复同一合法页的幂等重试可以支持，但不能混入不同遍历状态。

## V6 — 工具结果缓存断点使用错误内容块类型

S2b 已将普通字符串 message content 改为 input_text 块。但 `build_responses_wire_request` 的 function_call_output 分支把 output 字符串包装为：

```json
{
  "type": "output_text",
  "text": "tool result",
  "prompt_cache_breakpoint": {"mode": "explicit"}
}
```

官方 Prompt Caching 指南列出的 Responses 支持类型是 input_text、input_image、input_file。该工具返回作为下一次请求的输入时，应采用端点允许的输入类型。部分不支持形状分支还保留 input-item sibling fallback。

当前 `endpoint_shape_tests` 的工具结果用例只检查“数组中存在断点/顶层没有断点”，没有断言块类型；函数名与注释还保留 keeps_the_sibling_placement，与断言相反。这能解释为什么形状修正后错误类型仍可通过测试。

影响边界：显式在工具结果上放断点的 adapter 分支。不能据此声称当前默认只在策略/证据段放置 B0/B1 的每次生产请求都会失败。本轮没有调用真实端点，不能报告实测 400。

最小修复：为受支持 input/output 内容块做类型化映射；无法合法承载断点时剔除 hint 并记录原因，或拒绝该缓存计划，保留实际消息与工具配对，不回退到未经确认的专属字段。网关方言必须显式独立，不能假装是官方形状。

回归：独立 fixture 校验 block type、位置、合法角色、工具调用展开后的边界映射；不只断言字段存在。无需付费可完成，实际接受/命中/任务降本仍是条件实验。

## V7 — 取消时已知用量仅进入可选 observer

当前 retry 会合并失败尝试的已知用量到最终成功/失败，这部分已改善。但 live backoff cancel 分支记录 CallStage 后返回普通 AgentError::Cancelled；terminal_error 对取消也剥掉 usage wrapper。

生产 composer 使用 JsonlRetryObserver::from_env。OPENAI_RETRY_METRICS_FILE 未设置时 append 直接返回，因此 CallStage.known_usage 没有正式去向；Runtime 对普通 Cancelled 映射 OperationOutcome::Cancelled，也没有获取该用量。

条件反例：第一次尝试返回带 usage 的可重试失败 → 正在等待重试时用户取消 → 不设置可选 metrics 文件。程序已经知道的费用不应退化成无法取得的取消用量。此处不声称未知账目一定被按零显示，也不要求推断供应商未报告的费用。

最小修复：沿已有模型 operation 结算入口把 outcome 与 usage 分开表达，取消仍保持 Cancelled 分类和安全屏障，已知使用量走必有的结算通道；可选 JSONL 只负责诊断副本。不要简单改成失败包装后破坏 actor 的取消分支，不另建事件数据库。

回归：Compose → Retry → backoff cancel，metrics 环境变量不存在。核对前次已知用量保留、取消分类不变、未执行的重试不冒充已执行、未知字段不补零、同次 SSE 累计快照不重复相加。

## 维护性与实施顺序

建议仅做四个合并切片，沿当前 A/B/C 主线，不另建大阶段编号体系：

| 切片 | 内容 | 可维护性退出条件 |
|---|---|---|
| B：逻辑 owner 与冷页解析 | V1、V2；先保守保护，再补目标解析 | 所有权/引用问题不再由每个调用者手写四层枚举 |
| B/C：原子搜索页与 continuation | V3、V4、V5 | 一次查询结果自带 coverage；fresh/resume/restore 有一套生命周期规则 |
| C：供应商缓存映射 | V6 | 一处合法块类型映射和验证，无未经确认的 fallback |
| A/C：模型调用结算 | V7 | Cancelled/Failed/Success 与已知用量正交，诊断文件不拥有唯一事实 |

短期优先 V1。其他切片可以并行；共享 ContextEngine/service wire/ModelRequest 契约由单一集成人维护。定向回归通过后继续主体，阶段合并时跑既有相关集成，不为每个补丁重新组织完整付费实验。

运行建议（本环境未执行）：

```sh
cargo fmt --check
cargo test -p context-simple
cargo test -p context-contextcore -p agent-context-service -p agent-core
cargo test -p provider-openai -p agent-compose
cargo test -p agent-host --test host_process_variant
cargo clippy --workspace --all-targets -- -D warnings
```

命令不是新增反例已经存在或会通过的声明。先把上述行为测试加入既有 harness，再执行相关测试；整仓运行留在合并/阶段收尾。

文档只同步真实状态：CURRENT 中“已新增独立进程旅程”与旧的“尚无跨进程连续轨迹”冲突应一次替换；不要再把本报告全文追加到 CURRENT/NEXT_TASKS。GUI 仍仅必要兼容。供应商账单/真实命中/净费用测试没有执行时继续标 NOT_RUN。

## 固定版本来源索引

以下链接固定到本次 SHA；范围表示本轮实际读取区间，不表示整文件已完整读完。

- [scope 生命周期与退休](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/context-simple/src/scope.rs)
- [Context 引擎及 continuation / restore](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/context-simple/src/engine.rs)
- [必需正文规划](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/context-simple/src/materializer.rs)
- [full GC](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/context-simple/src/gc/full/mod.rs)
- [进程外适配器](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/context-contextcore/src/adapter.rs)
- [服务处理器](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/agent-context-service/src/lib.rs)
- [Context 契约](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/agent-contracts/src/context.rs)
- [供应商 mapper](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/provider-openai/src/lib.rs)
- [mapper fixture](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/provider-openai/src/prompt_cache/endpoint_shape_tests.rs)
- [retry / observer](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/provider-openai/src/retry.rs)
- [生产组合入口](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/agent-compose/src/lib.rs)
- [Runtime 模型 operation](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/agent-runtime/src/actor/model.rs)
- [独立进程旅程](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/4f6eb7ff7c72a592e38363d75a2c7907d4fd7466/crates/agent-host/tests/host_process_variant.rs)
- [固定 SHA 的 CI](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/35019429861)
- [OpenAI 官方 Prompt Caching 指南](https://developers.openai.com/api/docs/guides/prompt-caching)；本轮读取的支持块定义与该固定源码比较，官方页面可能继续更新。
