# TB8 根因修复与受限复测

基线：`ece82e04fee63625675248fb80d638b02cdcdd26`，分支 `codex/v5-audit-closeout`，包含既有未提交工作。用户要求“找到最根本原因”“修复问题然后继续”。本轮不修改旧 job、模型产物或官方成绩。

## 已确认的原因与旧报告纠正

1. **TB5 Rolling 的提前停止有 Runtime 原因。** 第一轮已提出 `task.complete`，被 `verification_not_current`、`acceptance_undeclared`、`operator_closure_only` 拒绝。随后普通源码读取也消耗完成修复的 6 次动作额度；第 4 轮工具列表为空，原因是 `completion_finalization`。不能归因为“模型第 4 轮主动放弃”。OperatorClosureOnly 保持不变不是普通开发动作没有价值的证据。
2. **TB6/TB7 的 PROTOCOL_INVALID 归类不成立。** CLI 累积审批拒绝，最终退出 3，但该事实本身不终止当时的模型循环。Harbor 即使记录 `NonZeroAgentExitCodeError`，仍收集产物并运行独立 verifier。TB6 有 28,741 字节 patch，验证安装 `psycopg2-binary==2.9.9` 失败；该槽官方 reward=0，没有 CTRF。TB7 实际运行至 40 轮、修改多个 API 文件，官方 CTRF **2/18 passed、16 failed**，reward=0。
3. **TB7 的 read-probe 并非不可用。** 模型通过 `process.run` 成功调用脚本 10 次，后来也发出不存在的工具名和越权写入。最终代码含未执行实际数据迁移的 stub，数据库表缺失。不能只用“缺少正式 tool schema”解释整个失败。
4. **确认 provider 账户凭据进入过模型可见输出。** TB6 `env` 调用是授权且成功的，子进程继承 agent 的环境；事件 89、237 的输出包含账户密钥。核查只输出存在与否，不复制密钥。前述“env 未授权被拒”描述错误。
5. **手工 fault variant 的 18/18 只证明该产物通过该 verifier。** 运行器执行批量复制，但没有证明持续负载期间主 Gunicorn 已切到 PostgreSQL，也未证明 CDC/逐批持久 checkpoint 完整。早先“完整在线迁移 coordinator 已验证”的表述过强；其手工证据仍与模型成绩分开。

证据目录均在 WSL `/home/ye_luo/.cache/context-agent-terminal-bench/jobs/`：

- `tb5-r-live-reeval-40-20260922/live-database-cutover__WXvpkZf`
- `tb6-r-live-closure-40-20260922/live-database-cutover__496YZe2`
- `tb7-r-live-broker-40-20260922/live-database-cutover__dBWQih6`
- `tb4-fault-live-performance-compat-d-20260922/live-database-cutover__wPeSQ7Q`

## 修复

- `agent-runtime/src/actor/turn.rs`：完成被拒之后，仅完成控制动作 `task.manage` 或当前 typed repair plan 指定的 resolver 累加完成停滞额度；普通读、写、进程工作继续受正常回合/操作预算约束。重复完成申请仍有独立上限，Core 的完成条件不放宽。
- `agent-runtime/tests/turn/completion.rs`：真实 Runtime 场景，一次完成拒绝后连续读取 8 个文件，确认工具仍可用、没有持久完成记录、没有触发错误终态；既有重复完成、伪进度循环仍终止。
- `tool-runtime/src/child_env.rs`：进程、session、shell、git、Python 解释器探测移除已知 provider 凭据环境变量。真实子进程回归确认凭据缺失且数据库配置保留。这是防止意外继承，**不是同用户进程隔离**。
- `credential_relay.py` 与 Harbor adapter：账户密钥只留在宿主 relay 内存，Harbor/task 仅获得随机临时 token。relay 只接收指定模型的 Chat endpoint，请求数、输入字节、输出 token 均有限制；拒绝重定向；不记录请求/响应正文。未知用量保持 unknown。
- prompt 去掉与现有 Python 执行 grant 矛盾的依赖安装禁令，不再宣称 Python 是只读沙箱，不再宣称 verifier 具有 Core 的持久完成权威。
- `outcome.py`：运行状态、Harbor 异常、官方 reward、CTRF 分开读取，不把 exit 3 自动变成“无验证”。

先前已输出的账户密钥不能通过本地补丁撤销。历史证据含敏感信息，保留本地；账户侧轮换尚未执行。

## 当前验证

- `cargo test -p agent-runtime --test turn completion:: -- --test-threads=1`：35 passed。
- `cargo test -p agent-runtime --lib --quiet`：451 passed、0 failed。
- `cargo test -p agent-runtime --test turn --quiet`：166 passed、0 failed、1 ignored。
- `cargo test -p tool-runtime child_env::tests -- --nocapture`：2 passed，含真实子进程。
- `cargo test -p tool-runtime --lib -- --test-threads=1`：313 passed、1 ignored，334.29 秒。
- `python -m unittest discover -s scripts/tests -p test_terminal_bench*.py`：10 passed。
- `cargo fmt --all --check`：通过。
- `cargo clippy -p agent-runtime -p tool-runtime --all-targets -- -D warnings`：通过。
- Debian bookworm 容器重建 `agent-tui` 成功，实际二进制摘要在新窗口 identity 中。
- 初次容器连通性 smoke 被任务 image entrypoint 接管，超时，0 provider 请求；改为显式 Python entrypoint 后通过。首次失败 receipt 保留。
- `python scripts/doc_consistency.py`：13 live docs 的结构/链接/工具链检查通过；`git diff --check` 通过。WSL 预检报告 `STATIC_READY`，检测到 Docker 29.1.3、Harbor 0.23.0 和实际 Debian12 二进制，但 `functional_smoke=NOT_RUN`、`trial_ready=false`，命令保持退出码 2，不授权开窗。本轮为相关代码与调用链的定向核查，不是完整全仓审查；未重跑 workspace 全套。

## TB8 独立窗口

同原始 live task，Rolling → Dynamic，concurrency=1，各最多 40 模型回合/40 上游请求，agent timeout=1200 秒；输入上限 262144 字节/请求、输出上限 8192 tokens/请求。模型 `deepseek-flash`、Chat、thinking disabled，context proposal 32768，模型维护调用/用量=0，自动重试=0。不注入手工 variant 或隐藏解题反馈。

任务仍是 8 小时官方任务，本窗口是 **40 轮诊断复测**，不代表完整默认预算 benchmark。此次同时修复 Runtime、凭据路径、prompt 与状态统计，不是单因素质量或 R/D 优劣的统计实验。

证据：WSL `/home/ye_luo/.cache/context-agent-terminal-bench/tb8-repaired-pair-20260922-run2/`（identity、每槽 config、relay usage receipt、Harbor 日志、分层结果）。首次零供应商 smoke 失败在无 `-run2` 的同名目录。

状态：两槽已结束。二进制 SHA256：`eeebb1603f19ad28651f9af28fce191b6f4b8c572ef5392647bd9cab1c5a2030`。
两槽官方 task checksum 均为 `84d06a18a91e9bd479863d29fe1c300d892b8013c051c7ab4c00fafa0163251c`，与 TB7 相同。

| Arm | 官方 reward | 运行事实 | 官方失败原因 |
| --- | --- | --- | --- |
| Rolling | 0 | 40 个 model_started 事件；此前有审批拒绝，CLI 最终 exit 3；relay 40 次成功上游调用后拒绝额外请求，最后是本地请求预算触发的 HTTP 429 | fresh verifier API 启动 `ImportError: cannot import name 'conn' from 'api.db'`；没有 CTRF |
| Dynamic | 0 | 40 轮，TurnCompleted、CLI exit 0；没有审批拒绝；`task_completed=false` | fresh verifier API 启动 `KeyError: 'MYSQL_HOST'`；没有 CTRF |

这两个 reward=0 均是独立 verifier 给出的成绩，不因 CLI exit 非零改写为协议无效。Dynamic 的 ordinary completed 也没有伪造 Core 持久完成。候选的跨文件接口与 MySQL 依赖清除尚未完成，不能宣称真实迁移任务通过。本轮没有人工修改这两份模型产物，也没有追加重试窗口。

供应商 usage（relay 实际收到，非费用估算）：

| Arm | 上游请求 | input | output | cached input |
| --- | ---: | ---: | ---: | ---: |
| Rolling | 40 | 591770 | 18000 | 180224 |
| Dynamic | 40 | 519936 | 7581 | 172544 |
| 总计 | 80 | 1111706 | 25581 | 352768 |

80 次均有 usage。没有取得供应商金额账单，不推导收费或 R/D 质量优劣。
两槽 job 与 TB8 receipt 共 **49 个文件**进行了实际账户密钥完整字节扫描，未检出；该检查不输出密钥。
Docker 容器列表为空，relay 随两槽结束关闭。

评测完成后对 relay 再补 `n=1`、禁止 `best_of` 与仅允许 streaming 的参数约束，防止临时 token 请求多候选绕过单次输出上限；Python 9 项回归再次通过，包含真实 HTTP 重定向不跟随测试。该补充未发起新供应商请求，评测时 relay 的旧摘要仍保留在 identity，不将新源码冒充已评测的同一哈希。
静态 preflight 现在只报告 `STATIC_READY`，要求 Docker/Harbor 版本探测成功，并明确 `functional_smoke=NOT_RUN`、`trial_ready=false`；不再以命令存在或源码含 state-dir 字样证明功能就绪。

缓存清理已完成：先验证目标解析为精确的 WSL `tasks-v4/live-database-cutover` 目录且不是链接，再删除该临时任务包；清理后目录不存在。job/receipt、fault variant 和 Docker 镜像完整保留。

本地修复与该受限复测已收口，模型任务仍未验收。未提交、未推送，未运行远端 CI。
