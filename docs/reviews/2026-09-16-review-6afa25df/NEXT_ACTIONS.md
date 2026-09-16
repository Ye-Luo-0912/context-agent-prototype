# 实施任务：6afa25df 后端闭环

基线 `6afa25dff0230fec982ee7d836aa7811ad901d0c`。本文件是本轮建议，不代表已提交代码。全量仓库逐行覆盖未完成；所有新测试均尚未在本环境执行。

## 接手前

核对 HEAD、工作树及并行修改，若目标 HEAD 有变化先复核受影响函数；不得 reset 他人未提交内容。共享 contracts/usage/ModelInput 由单一集成人合并。保留已有 B1/B2、R 系列有效修复，只补本次下一跳缺口。

## 切片 A — 冷正文从预览到消费成功（B 主责，Runtime 集成）

用户动作：在固定小热目录下，任务要求 A/B/C 正文并让模型继续执行；无需扩大热预算。

1. 在现有 `batch_required_plan` fixture 中追加真实 `ContextConsumptionAck`，先证明旧 owner 计数拒绝 A。保持 A 确实留在 pending 的断言。
2. 设计一个统一逻辑 owner 查询，涵盖四种已加载 owner 与 cold locator；检查身份/版本/重复 owner。
3. 把 pending 消费戳纳入同一结算路径；不能只改布尔计数，不无限 pin，不用全量 hydration。
4. 通过真实 Runtime 将最终帧发给受控模型，再提交 ACK 和结果；校验无不必要失败，后续 checkpoint/restore 一致。

停止条件：正反例都成立，热预算/取消安全未退步；不重写整个 ContextEngine。

## 切片 B — 一次模型尝试只有一个使用量结算出口（A/C）

用户动作：模型已返回部分或完整计数，后续流/内部提交失败时，正式账目仍保留这些数值。

1. Runtime 注入 ACK 错误，验证 `ModelUsed` 不因业务结果被拒绝而消失。
2. Provider 用本地 SSE fixture 在 usage 后分别注入 idle timeout、坏帧、I/O、cap 和 sink failure。
3. 将错误分类和使用量结算分开，统一读出 accumulator；同 attempt 的累计快照不重复相加。
4. 经过 Retry 与取消/迟到结果验证全部真实尝试计数，不依赖可选 observer 文件。

停止条件：已知数值保持、未知不补零、业务拒绝保持、重复 completion 不重算。供应商付费调用不是这个切片的前置条件。

## 切片 C — SDK 的事件流健康与重同步屏障（C）

用户动作：客户端自己落后造成 Session 队列溢出，底层 socket 仍正常，也能从公开入口恢复实时事件。

1. 复用 `Overflowed_session_rebuilds_its_event_stream_on_reconnect_and_delivers_new_events`，去掉服务端 dropFirst，持续回答查询。
2. 把 `_eventsOverflowed` 纳入 session 可用性/恢复入口；明确新事件 reader 的代际。
3. 快照、队列与 connection 代际统一安装；新 pump 不先于快照生效交付；旧 pump 的校验与入队不能跨安装。
4. 用受控屏障覆盖两个 Q5 interleaving，不用延长 sleep 代替验证。
5. mutation/审批未知结果不自动重发，旧 reader 不复活。

停止条件：健康 socket 下恢复成立，正常断线重连也不回归；无旧代际通知污染、无新事件被旧快照覆盖。

## 切片 D — TUI worker 正常与异常退出共同收尾（A）

用户动作：绘制/键盘出错或用户退出时，不再出现前端已结束但排队动作继续提交。

1. `run_session` 内部循环返回 Result，外层执行共同清理；异常/正常路径同一个入口。
2. 命令状态区分 queued、已取走、已送到 Runtime、结果未知；worker 提供停机回执，counter 仅作诊断。
3. 修正 pending 增减时机及退出读取竞争，取消安全结算 capture/restore。
4. 注入 UiSink/UiSource 错误、正常 quit、缓慢磁盘/已发 Runtime 命令；验证 stop 屏障与 join。

停止条件：worker、Runtime、terminal 三类清理各自可核对；不把 abort 冒充未执行或回滚。

## KV 本地序列验收（与上述切片可并行准备）

固定任务、模型/profile、工具契约、窗口预算；使用最终请求，不用手填 DTO。

| 变化 | 应核对 |
|---|---|
| 仅焦点/当前进度改变 | 稳定基座边界摘要不应无故变化。 |
| 新检索/缺失提示 | 动态段可以变化；必要正文实际进入请求。 |
| 必需文件版本改变 | 缓存视图立刻失效或重建，不保旧事实换命中。 |
| 工具撤销/替换 | 实际 schema 与权限及时改变；旧计划不被误用。 |
| checkpoint 恢复 | 稳定路由身份与有效证据保持；没有第二份无界历史。 |
| SSE/ACK/取消失败 | 已知 usage 不丢，未知独立；各真实 attempt 可核算。 |

本地前缀相同不是服务端命中证明。真实供应商实验另按授权预算执行，端点接受、命中、净费用三项分别记状态。

## 建议执行的已有检查入口（本轮 NOT_RUN）

```sh
cargo test -p context-simple batch_required_plan
cargo test -p agent-runtime
cargo test -p agent-compose
cargo test -p provider-openai
cargo test -p agent-tui

dotnet test clients/dotnet/Agent.Client.Tests/Agent.Client.Tests.csproj --filter FullyQualifiedName~EventStreamTests

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

先运行新增定向用例及相邻集成，再使用已有 workspace CI。命令是建议入口，不代表本环境执行成功；不自动启用真实模型、不读取或输出凭据、不产生外部提交。

## 文档收尾

NEXT_TASKS 只保留实际待执行动作与回执指针；不要把全部 Q/O 描述再次复制进去。关闭范围要覆盖调用链下一跳，而不是只有某个 helper 的绿测。资源采样 O1 与卡片读取硬界 O2 作为后续小切片，不阻塞主体任务试用。
