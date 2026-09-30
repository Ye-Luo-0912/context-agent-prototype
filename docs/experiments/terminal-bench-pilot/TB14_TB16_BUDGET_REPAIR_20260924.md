# TB14–TB16 输入预算修复与真实任务回执（2026-09-24）

三次窗口彼此独立，均使用固定的 `live-database-cutover@4.0.0` 题包、
`deepseek-flash`、宿主凭据 relay，以及只读挂载的有限 Core grant。
每次另写 identity、Harbor job、relay 账本；不改写 TB9–TB13 的结果。
题源 raw `task.toml`/`instruction.md` SHA256 分别为
`3d92be60d134825cdc7c0dc70603938c912200f96cb9c4ffb9d8a297192b5941` /
`7c466c3b181bdda2751d8af68af5d030802bce94d3e9ece62d755af4aa9ec27e`；
Harbor 本地规范化 `task.toml` 为
`771fb5d6703e63a80d9d468f21d8f090be9df95c2262461ecfe0ac318618a540`。
环境镜像 ID 固定为
`sha256:dbd2997e95d6bdde237f94cefb8cdc45ef5c7bbd44ecde02052a6dffcde14054`。

## TB14：真实 Runtime 固定层越界

TB13 的失效原因是评测授权过窄。TB14 的新 grant
`grants_tb14_live.json` SHA256
`58edcc376110ef2bd12a27053dc5072eea03a609967ba37588e00f4e993c7308`
只为这道隔离题的工作区增加 `entrypoint.sh` 与 `migrate_archive/`
写入范围；加上 `api/`、`_verify.py`、`migrate.py`，与 harness 告知模型
的范围一致。`python3` 仍是有限 400 次，原到期时间不延长。
runner 新增 journal 在飞观察，1400 次工具尝试或首次真实审批拒绝
就中止配对；WSL 合成 Harbor 子进程验证首次拒绝后数秒内收到 SIGINT。

Dynamic 完成 36 次供应商请求后，第 37 次请求被 Runtime **在发送前**
以 `input_budget` 拒绝：Context 正文已经清空，可选工具 schema 也已
略去，最终保守估算仍为 **24,922 > 24,576** input token。
`ToolSurfacePlanned` 的当轮数字：selected/mandatory schema 均为 979
token，Context selected 数为 0；剩余主要是系统/任务/本轮工具对话等
固定消息层。该槽有 61 次工具尝试、无审批拒绝，Runtime exit 1；
Harbor 因非零 agent exit 记 reward 0，但没有 CTRF，不能当作已完成
官方 verifier 的质量分数。runner 当时仍会启动第二槽，故手动中止
Rolling；它已结算 23 次供应商请求，没有正式成绩。两个槽的 relay
usage 都已结清、无未知预约。

根因在 `agent-runtime/src/actor/model.rs`：TB10 的多留完整历史会先
按 send-only 余量选择尾部长度，Context 最终裁剪只移除 Context
正文和可选 schema，不再缩短已经选定的完整工具交换尾部。修复让
Runtime **在 Context 查询前**先用空 Context、当前进度投影、可回注
协议正文与实际工具 surface 装配最小请求；若固定层临近发送上限，
逐组缩短完整交换尾部，再以同一个长度计算 Context 可见正文提示、
Context 预算与最终 prompt。全量 TurnFrame 审计不变，压缩后的
checkpoint 注记仍保留。只有真实请求预算紧张才多压缩，不把
历史固定减到更短。

本地行为回归使用 12 次连续 `fs.read`、每条约 17 KB 正文，真实
Runtime 必须发送后续请求，保留最近完整结果并对较早结果生成
checkpoint；旧策略在类似固定层压力下会触发 `input_budget`。
`cargo test -p agent-runtime --test turn --quiet`：172 passed、1 ignored；
`cargo test -p agent-runtime --lib --quiet`：452 passed；
`cargo fmt --all --check` 与
`cargo clippy -p agent-runtime --all-targets -- -D warnings`：通过。
Windows Cargo 只出现既有增量编译目录垃圾回收的拒绝访问警告，测试
退出码均为 0。修复后的 `agent-tui` 用 Rust 1.97 Debian12 容器
在无网络模式重新构建，随后在无网络 Debian12 容器执行 `--help`
成功。新 ELF SHA256
`d8adc91941807c9a3c7995ad69e2fdb3eac13809262f4612655b3e824f03fd29`，
在工作区忽略的 `target-debian12/archives/` 按哈希独立保存；
旧 TB10 ELF 曾在可变构建路径，重编译已覆盖该路径，TB11–TB14
保留了其身份哈希和任务证据，但本地未单独归档当时的可执行文件。

## TB15/TB16：真实供应商回归未到预算故障点

TB15 用新 ELF 做 64 决策配对。Dynamic 的首个请求进入 relay 后
上游流发生 `IncompleteRead`，未取得 usage；relay 按保守费用门
冻结后续请求，模型重试收到本地 HTTP 429，Runtime 报
`provider_transport`/exit 1。该请求保留 input 15,832 / output
8,192 的预留，是否实际计费未知。旧 runner 尚未阻止首槽失败后
启动 Rolling；它在手动中止前有 16 次结算请求与 1 次
`ConnectionResetError` 未知在途请求，后者预留 input 99,487 /
output 8,192；Rolling 无成绩。两槽相关进程与容器已退出。

随后 runner 补上首槽可比性门：Runtime 未正常结束、Harbor 异常、
无独立官方成绩或 runner 自身中止时不启动第二槽。本地 Python
Terminal-Bench 测试 20 项通过（Windows 1 项 POSIX SIGINT 用例
按平台跳过）；WSL pilot 测试 15 项全过，包含合成 Harbor
审批拒绝后中断。TB16 是新协议下的 64 决策回归：Dynamic 6 次
请求全部结算、无未知用量，但第 6 次模型流形成了第 1 号工具调用
的不完整/非法 JSON 参数；严格 parser 返回
`malformed-tool-call`，Runtime `model` failure/exit 1，
Harbor reward 0 且无 CTRF。runner 正确标记
`RUNTIME_NOT_COMPLETED` 并**没有启动 Rolling**。这不是输入预算
回归，也不能把供应商不完整工具参数自动当成已获授权工具执行。

| 窗口与槽 | 已结算请求 | input / output token | 峰时全未命中已结算估算 USD | 未知在途 |
| --- | ---: | ---: | ---: | --- |
| TB14 Dynamic | 36 | 722,759 / 19,807 | 0.240596 | 0 |
| TB14 Rolling（手动中止） | 23 | 543,972 / 10,253 | 0.175495 | 0 |
| TB15 Dynamic | 0 | 0 / 0 | 0 | 1 |
| TB15 Rolling（手动中止） | 16 | 332,043 / 6,569 | 0.107496 | 1 |
| TB16 Dynamic | 6 | 60,864 / 1,126 | 0.019610 | 0 |

估算采用 [DeepSeek 当前价格页](https://api-docs.deepseek.com/quick_start/pricing/)
的 Flash 峰时缓存未命中输入 $0.30/百万、输出 $1.20/百万；
它不是供应商实际账单。TB15 两笔未知在途请求不计入上表已结算
金额，也不能假定零费用。relay 的 wire 字节预留没有官方严格
token 上界，因此不称为无条件金额硬封顶。DeepSeek
[状态页](https://status.deepseek.com/) 在核查时显示整体正常，
但不能推翻 TB15 单条流的 `IncompleteRead` 事实。

TB14/TB15/TB16 分别扫描 44/44/26 个新 evidence/receipt/job
文件的宿主账户密钥完整字节，三组结果均为
`account_credential_present=false`。三组 evidence 保存在 WSL
`/home/ye_luo/.cache/context-agent-terminal-bench/` 下各自命名目录
及 `jobs/`，没有改写历史证据。当前结论是：输入预算的代码根因与
本地行为修复已验证，修复后真实 DeepSeek 长回合是否越过第 37 轮
**仍未验证**；TB15/TB16 分别被供应商流中断和非法工具 JSON
先行阻断，正式 400 决策的质量比较仍不可给出。
