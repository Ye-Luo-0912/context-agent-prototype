# KV 共同布局第一切片

**后续实测更新：**用户授权复用本地供应商环境后，已执行 [10 次有界真实请求](../2026-09-10-cache-live/REPORT.md)。合成焦点/证据回答全部正确；供应商缓存可命中，但本次未证实新布局在变化状态下更省费用。下文 NOT_RUN 保留为本切片代码完成时的事实；实际账单、写入计费和真实代码任务质量仍未验证。

实施基线：`6ec044a` 上的工作树。用户要求供应商共同底层优先，并尽量保留当前焦点和上下文策略。

## 用户结果

普通 Runtime/compose 请求采用 `CurrentStateLast`。完整的当前工具目录与 Focus/TaskAnchor/TaskProgress 移到请求末尾，前面的证据和工具交换不再单纯因为进度更新而重写。完整原始用户指令仍保留在 TurnFrame，当前焦点每次重新生成，不复用过期状态。

这一步只改变消息位置。未改变 ContextQuery、焦点评分、候选顺序、GC、语义/驻留状态、正文去重、回注、工具选择、schema 顺序和最近 6 组协议窗口。也未拆除正文标题中的状态/诊断信息；当那些信息真的变化时，前缀仍会变化。

## 实现

- `ModelInput` 新增 serde-compatible `layout` 与 `current_state_frame`。旧记录缺字段时按 Legacy 还原原有顺序；新 PromptAssembler 默认 CurrentStateLast。
- `focus_frame` 保持原有完整渲染；目录移入类型明确的 `current_state_frame`，保留 System 角色。正文仍为原低权限角色；工具调用和结果不拆组。
- `RuntimeServices::with_prompt_layout` 和 `agent_compose::compose_with_prompt_layout` 提供 Legacy 回退，不新增远端命令或环境变量。
- 模型请求 metadata 记录 `prompt_layout`，不进入正文，也不作为供应商参数发送。目录 token 账目改从当前状态帧读取；最终预算与缺失检查仍走原路径。

## 验收重点

1. 同一输入在新旧布局下的消息内容与角色逐字相同，选择顺序、schema、正文范围与 checkpoint 相同。
2. 进度变化只改变末尾块，未加载目录状态仍及时更新，不能为缓存冻结工具可用性。
3. 真实 Runtime 路径中切换任务焦点后当前帧不残留上一任务目标，超过 2,000 字符的完整指令仍在请求中。
4. Chat Completions / Responses wire builder 都保留尾部状态和完整 tool-call/result 对；这不代替真实 endpoint 接受性验证。
5. 旧序列化输入可读，旧布局可选，必需正文缺失依旧阻止完成。

## 验证与边界

实际执行：

| 命令 | 结果 |
|---|---|
| `cargo test -p agent-runtime --lib prompt::` | 30 passed |
| `cargo test -p agent-contracts --lib model::tests` | 11 passed，含旧输入解码与完整工具组 |
| `cargo test -p provider-openai --lib builds_` | 2 passed，含两种 wire 的末尾状态 |
| `cargo test -p agent-runtime --lib actor::model::failure_class_tests` | 9 passed，含 required_miss 撤销 settlement |
| `cargo test -p agent-runtime --test turn` | 120 passed；1 条旧消息位置断言失败，按新布局更新后定向补跑通过（下一行），未把首次命令记作全绿 |
| `cargo test -p agent-runtime --test turn scopes::turn_frame_is_execution_stack_not_long_term_memory` | 1 passed，保留原协议、ingest 和维护时序断言 |
| `cargo test -p agent-compose --test product_flow` | 1 passed，保存/重启/继续只写一次 |
| `cargo clippy -p agent-contracts -p agent-runtime -p agent-compose -p provider-openai --all-targets -- -D warnings` | exit 0 |

新提示组装测试首次有一处夹具问题：空 TaskProgress 不渲染 focus，添加首条事实后消息数自然增加；已改为两次均有非空进度的配对输入。旧回合测试有一处把旧角色位置写死，已更新为新顺序并保留 Focus 内容及工具协议断言。

四个涉及 crate 的 `cargo fmt ... -- --check` 通过。全仓格式检查曾在共享工作树新增的 `context-baselines/src/lib.rs` 和 `tool-runtime/src/tools/artifact.rs` 各报告一处换行差异；这些不属于本次布局修改，未代为覆盖。文档与 `git diff --check` 另行检查，不宣称全仓测试或新 CI 已通过。

真实 provider 命中率、输入账单、LLM 任务质量均 NOT_RUN。信息逐字保留不等于 LLM 行为完全相同，旧布局回退因此保留。后续才考虑正文状态拆分、区段化协议窗口、供应商用量归一化和可选断点；不能为了命中率冻结焦点或扩大上下文上限。
