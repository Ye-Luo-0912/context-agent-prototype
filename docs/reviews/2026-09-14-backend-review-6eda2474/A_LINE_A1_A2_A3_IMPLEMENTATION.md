# 2026-09-14 后端续审 A 线实施回执（A1/A2/A3，工作树）

对应 2026-09-14 后端续审（基线 `6eda2474`）的 A 线三片：N07/N08（A1）、N09（A2）、N10（A3）。共新增回归 **10 项**（其中 2 项 unix-gated，由 CI Linux 作业覆盖）。**未提交/未推送、未跑远端 CI。**

## 实际执行的验证（本机 Windows）

| 目标 | 结果 |
|---|---|
| `cargo test -p tool-runtime --lib` | **277/277**（含本轮 8 项；2 项 unix-gated 不在本机执行） |
| `cargo test -p agent-capability-process --lib` | **27/27** |
| `cargo fmt` / `clippy --all-targets`（tool-runtime、agent-capability-process） | 0 diff / **0 警告** |

## A1（N07/N08）：process.session 状态分离、批次硬界与短注册表锁

1. **N07 终态事实化**：新增 `ProcessTermination`（`Exited{code,success}` / `Signaled{signal}`，unix 信号从 `ExitStatusExt::signal` 提取）。会话记住首次观察到的终止——信号杀死（`code()==None`）不再被 `Option<i32>` 误判为 running；退出事实一经记录不可回退为 running。
2. **N07 模型可见性**：poll 的 `model_content` 携带 `success (exit code 0)` / `failure (exit code 7)` / `terminated by signal N` 标签——TurnFrame 只转发 model_content，模型无需从 metadata 反推成败；metadata 增 `signal`/`success`/`output_pending`。
3. **N08 批次硬界**：drain 的初始非阻塞扫描受 chunk 数（8192）、字节数（8 MiB，按 capture 累计差值）与绝对批截止（2 s）三重上限，循环内检查 cancel；**退出后排空改用单一绝对截止（1 s，不再按每条 chunk 重新计时）**——持续写入的后代无法无限延长 drain。任何截断都诚实置 `output_pending`，余量留在 channel 由下一批继续（不丢弃）。
4. **N08 短注册表锁**：`SessionSlot::Running` 改持 `Arc<tokio::sync::Mutex<ProcessSession>>`——poll/stop/drain_sessions 只在表锁内克隆/摘除句柄，drain 与 artifact flush 只持有**该会话自身**的锁；其他会话的控制路径不再排队。
5. **回归 6 项**：`a1_nonzero_exit…failure`、`a1_zero_exit…success`（模型内容区分 exit 0/7）、`a1_signal_killed…`（unix-gated，信号退出不再 running 永远）、`a1_continuous_writer…`（2 万行爆发：批界内返回＋多批无丢失排空）、`a1_post_exit_continuous_writer…`（判定性：父退出后 300ms 周期的持续写后代——旧实现按次重计时 drain 无限延长，新实现单绝对截止内返回 `output_pending=true`）、`a1_stop_of_another_session…`（poll A 进行中 stop B 不被拖住）。既有 R05/R11 全部生命周期回归保持绿。

## A2（N09）：退出后排空宽限从实际退出时刻起算

`shell.rs` 与 `process.rs` 的同一控制流：`child.wait` 首次观察到退出时 `grace.as_mut().reset(now + 500ms)`——宽限窗口从实际退出时刻起算，而非启动时预挂（预挂 timer 在 >500ms 命令退出时早已过期，尾部日志丢失）。两处均新增 `pipes_drained` 标志并进 metadata：容量截断（`artifact_truncated`）与管道未排空（`pipes_drained=false`）是两种可区分的不完整，不互相冒充。**回归 2 项**：`n09_tail_ordering_guard`（head+尾部序守卫，双平台）；`n09_post_exit_descendant_sentinel…`（**unix-gated 判定性回归**：后代持有继承管道、父退出后 0.2s 写 sentinel——重置后 grace 收集到；旧实现 timer 已过期即丢。本机 Windows 不执行，CI Linux 作业覆盖）。

## A3（N10）：MCP tools/list 有界分页发现

`list_tools_with_cancel` 改为沿 2024-11-05 协议的 `nextCursor` 走有界分页循环：页数上限（16）、工具总数上限（512）、整体期限（30s）、重复 cursor 与重复工具名为类型化协议故障；取消与中途失败照常传播——**所有越界/故障都以「discovery incomplete / install refused」的类型化错误收场，绝不把首批静默当作完整 manifest**。adapter 的 connect 失败路径（poison + reap）不变。**回归 3 项**（红检查在先：短路分页时目标工具丢失、只回首页）：`paginated_discovery_finds_tools_on_the_second_page`（第二页工具被发现）、`a_repeated_cursor_is_a_discovery_fault…`（重复 cursor/重复工具名 → 类型化故障拒绝安装）、`a_second_page_failure_is_not_a_silent_complete_manifest`（第二页失败浮出为错误）。

## 边界与如实记录

- 未做「dispatcher → broker → TurnFrame」全链集成回归：模型可见性语义已在工具边界用 model_content 断言覆盖，TurnFrame 对 model_content 的转发是既有管道（N07 反例的根因在工具侧内容而非转发）。
- `n09_post_exit_descendant_sentinel…` 与 `a1_signal_killed…` 为 unix-gated，本机未执行；Windows 本地用 `n09_tail_ordering_guard` 与非信号路径测试覆盖。
- 每会话锁的并发窗口测试（`a1_stop_of_another_session…`）用 5s 时钟上界断言，非纯时钟消除型；与其他既有时序测试同类型。
- registry.rs 的 Drop 路径同步适配：无法 await 的 Drop 只在会话锁无争用时读 pid（try_lock），否则依赖既有 child-handle drop 杀直连子进程——与注释声明的 best-effort 语义一致。
