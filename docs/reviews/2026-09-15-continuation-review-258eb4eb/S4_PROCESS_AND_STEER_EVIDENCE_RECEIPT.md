# S4 回执：T7 补进程边界与指令传递证据

对应工单：`docs/NEXT_TASKS.md`「S4 — T7 补进程边界与指令传递证据（A，agent-host）」；
对应缺口：本目录 `REVIEW.md` 第 8 节（R6）。
日期：2026-09-14（本地执行）；工作树基线：`4ba60b97`（HEAD）。
只动 `crates/agent-host`：新增一个测试文件＋改一个现有测试文件的模块注释。无生产代码改动，无新依赖，无第二套状态权威；进程变体走 host 正常的本地服务面（与 T7 相同的 C0 wire），未开新后门。

## 现场（R6 原判）

T7 旅程（`host_t7_journey.rs`）证明了真实工作区写入、wire 审批、checkpoint 恢复与跨会话不重放，但两点不能由它声称：

1. server 用 `std::thread::spawn`，session-2 是同一测试进程内的重新 compose——新 RunId 不是新 OS 进程，宿主进程死亡后的全局状态（单实例锁重初始化、进程存活判定）未被行使；
2. 脚本模型固定生成 `Q:` 前缀、按轮次索引回放，steer 正文与恢复后剩余义务是否真的进入了模型请求，没有证据。

## 交付内容

### 1. 同进程旅程保留并准确标注（行为零改动）

`crates/agent-host/tests/host_t7_journey.rs` 只改模块级注释：明确它证明什么（同任务旅程语义：真实工具效果、wire 审批、steer 槽、可持久取消屏障、跨平面 checkpoint、同 TaskId/新 RunId 的恢复、不重放、OperatorClosureOnly 交付），不证明什么（新 OS 进程；宿主进程死亡后的单实例锁接管、进程存活、watchdog/监督重初始化——指向新测试文件）。两个既有测试的断言与行为一字未动。

### 2. 独立 OS 进程变体（新增）

新文件 `crates/agent-host/tests/host_process_variant.rs`，单一路程测试 `named_pipe_procvar_two_real_host_processes_restore_one_task`（Windows named pipe；`unix_socket_procvar_two_real_host_processes_restore_one_task` 同款归 unix）。要点：

- **真实 host 二进制**：`env!("CARGO_BIN_EXE_agent-host")` 启动真正的 `agent-host` OS 进程，配置走操作员路径：`--workdir`＋`--pipe`/`--socket`，模型经环境变量（`OPENAI_API_KEY`/`OPENAI_BASE_URL`/`OPENAI_MODEL`/`OPENAI_API_PROTOCOL=responses`）指向本地脚本 provider；`MAINTENANCE_MAX_CALLS_PER_MAINTAIN=0` 保证脚本模型永收不到引擎维护调用；`NO_PROXY` 按仓库既有惯例豁免环回；`AGENT_DEMO` 显式移除。
- **脚本 HTTP provider**：复用 `agent-compose` `cache_wire_flow.rs` 已验证的环回捕获服务器模式（127.0.0.1:0、OpenAI Responses SSE 形状），不新造第二套 mock 基建。区别在于它**按收到的请求内容决定响应**：脚本是一个纯函数 `script_step(round, body, goal, token)`，每轮先校验请求实际携带的事实，缺失即拒绝。
- **PID 证据链**（`spawn_host_process`/`wait_host_ready`/`kill_and_confirm_exit`）：
  - 进程 1 完成可持久工作（提交 → 模型真实 fs.write stage.md → wire 审批 → 效果落盘 → 正式 checkpoint artifact 在磁盘上）；
  - kill 进程 1（Windows TerminateProcess / unix SIGKILL），`Child::wait` 确定性观察退出，再用产品自己的存活探针 `agent_process::process_is_running(pid)` 断言 PID 不存在（这正是单实例锁判 stale 用的同一探针）；
  - 进程 2 启动前断言 `pid2 != pid1`；它必须接管被杀进程留下的 stale `host.lock`（锁重初始化路径），快照证实冷态（focus 无、任务空、RunId 不同），再 `work/restore` 从磁盘恢复同一 artifact——响应携带**被杀进程的 RunId**（lineage 跨真实进程死亡连接），同一 TaskId 重新聚焦，stage.md 逐字节存活，效果不重放（changes.jsonl 中 stage.md 恰 1 行）。
- **结束形态**：进程 2 的续作 ordinary final 后任务仍 `Active`（OperatorClosureOnly；跨真实进程边界无 in-process RuntimeHandle，操作员显式关闭不在本变体内，与现有旅程分工）。

### 3. 纠正传递证据（新增）

- **一次性唯一标记**：steer 指令携带运行时生成的 token（`pvtk-{nanos}-{serial}-{pid}`，SERIAL/nanos 命名先例同 t7），写死进断言：进程 2 恢复后写出的 `summary.md` 内容必须逐字节等于 `correction: {token}`——该内容只能来自运行时真正递交给模型的纠正文本。
- **provider 校验即门禁**：
  - 进程 1 被排空的纠正轮（round 2）请求必须同时含目标与 token，否则 provider 返回 HTTP 500（经 `RetryingTransport` 重试后该轮失败，旅程停在「运行时没把信息交给模型」，而不是脚本预设答案强撑）；
  - 恢复后进程的首个请求（round 3）必须含任务目标、已完成部分（stage.md）与 token（剩余义务跨 checkpoint＋进程死亡＋恢复存活），缺失同样 500；
  - round 3 的写入内容从请求体内**实际找到的** token 切片派生（`delivered_marker`），响应由请求决定，不是无条件回放。
- **顺序因果**：provider 与测试客户端共用一个有序日志（`ProviderLog`）。断言：steer 记录之前存在模型请求；steer 之前的所有请求都不含标记（因果方向）；steer 之后的第一条模型请求必须含标记。另断言全程 `refusals` 为空、总轮数恰为脚本约定的 5 轮。
- **拒绝路径可证**（"缺标记时测试真的会红"）：同文件 `script_contract` 单元测试 5 个，直接对纯函数 `script_step` 断言——round 0 缺目标拒绝、round 2 缺标记拒绝、round 3 缺标记拒绝、round 3 合法请求产出的 SSE 写入内容派生自请求中的标记、round 1 合法请求是 Hold 而非拒绝。拒绝是显式枚举分支（`Decision::Refuse` → HTTP 500），不是静默降级。

## 验收命令与结果

环境：Windows（Git Bash），HEAD `4ba60b97`，与并行分支（agent-contracts/context-simple in-flight 编辑）共享同一工作树与 target 锁。clippy 余下的 1 条 workspace 警告位于 `context-simple/src/index/external.rs`（并行分支正在编辑的文件），不在本切片范围。

- `cargo test -p agent-host --test host_process_variant`：**6 通过**（1 路程 `named_pipe_procvar_two_real_host_processes_restore_one_task` ＋ 5 个 `script_contract` 单元测试），单次 7.6–9.4s。日志摘录（首次运行）：两个真实 PID（30648 → 33984）、`host process 1 (pid 30648) exited (exit code: 1); pid confirmed gone`、checkpoint artifact 落盘、恢复后 `summary = "correction: pvtk-…"`、任务 `Active`。
- **连续 3 次重复全绿**（`-- --test-threads=1`）：9.36s / 8.19s / 7.88s，每次 6 通过 0 失败。另在 clippy 修正前的同语义代码上已有 4 次全绿（首次 nocapture 运行＋后台验证序列 3 次，8.63/8.24/7.62s），修正仅折叠 if 与把断言移出 async 函数的锁作用域，无行为变化。
- `cargo test -p agent-host --test host_t7_journey`：2 通过（9.51s）——既有旅程在注释修正后保持绿色。
- `cargo test -p agent-host`：全套 **31 通过 0 失败**（lib 8、host_config 3、host_e2e 9、host_process_variant 6、host_restore 3、host_t7_journey 2）。
- `cargo clippy -p agent-host --all-targets`：agent-host 全目标 0 警告（含新测试文件）。
- `cargo fmt -p agent-host -- --check`：clean。

## 限制（如实）

- **同进程旅程保留、独立进程变体新增**：`host_t7_journey.rs` 的覆盖（取消屏障、恢复后真实工具失败变体、操作员显式关闭）仍是同进程重组语义；进程死亡变体只覆盖 R6 点名的最小链（锁接管、存活判定、lineage 恢复、指令传递），不声称覆盖 watchdog/监督重初始化的全部路径。
- 脚本 provider 按轮次＋内容双门禁；若运行时在脚本约定之外多发起任何模型请求（如维护调用），拒绝分支会使其显式变红——这是契约的一部分，不是兼容缺口。维护调用已用 `MAINTENANCE_MAX_CALLS_PER_MAINTAIN=0` 确定性关闭。
- 不涉及真实付费模型、供应商 KV 命中与质量（T8 条件任务）；本变体的 provider 是环回脚本服务，不声称端点协议实测。
- unix 变体（UDS）按 t7 同款入口提供，本机执行的是 Windows named pipe 入口；unix 入口未经本机运行验证。
- 验证期间并行 Agent 对 `agent-contracts`/`context-simple` 的 in-flight 编辑使共享树间歇无法编译（约 39 分钟），本切片的全部验证命令在该窗口结束后、共享树可编译时一次性完成；期间未触碰并行分支的任何文件。
- 未执行 git add/commit/push（由主会话统一验收提交）。

## 主会话验收补充（2026-09-15，合入前）

- 共享树合并验收：`cargo test -p agent-host` 全套 31/0、clippy `--all-targets` 0、fmt clean。
- **新增修复（同套件稳定性）**：`host_e2e.rs` 的 `stop_is_bounded_with_no_client` 在满载并行下约每两三次全量运行出现一次 30s 假失败——`stop_and_join_bounded` 的唤醒连接用的是会 panic 的 `connect()`（30s 预算）；服务线程已自行退出、管道销毁后，该唤醒在死管道上空转满预算后 panic，抢在 join 裁决之前。修复：唤醒改为 fire-and-forget 循环 poke（不 panic），裁决权完全交给带界 join——服务线程真死时其真实错误经 join 呈现，而不是被盲转掩盖。本地复现修复前 2/5 失败 → 修复后 14/14 全绿（9 线程 6 次＋16 线程 8 次）。如实记录：修复后的第一次运行出现过一次未捕获信息的失败（8.25s，非盲转模式），随后 14 次极限并行未再现；若真实 serve 线程故障再现，现在的失败信息将是 join 的原始错误而非盲转 panic。

## 合入后 CI 追加修复（2026-09-15，run 35017012932）

- CI 的 windows part full 里进程变体旅程失败：「the first request after the steer must carry the marker: model round 1 arrived」。根因是证据账本把**到达序**当成了因果序：被 hold 的 round-1 请求由运行时在 steer 之前发出，只是其请求体在满载 CI 上尚未被 provider 读完记录；steer ack 先入账后，账本里「steer 后第一个请求」就成了合法无标记的 round 1。到达序在此处不表达运行时因果。
- 修复：账本断言改为按 provider 分配的轮次序号表达因果（round-1 的 handler 先记录后 hold、round-2 只在放行后发出，故序号稳定对应脚本角色）——round 0/1 不得含标记（纠正尚未存在）；从 round 2（被排空的纠正轮）起**每个**请求都必须含标记（运行时把纠正交给了模型，且该义务跨进程边界存活）。标记交付事实同时由脚本门禁独立强制（round≥2 缺标记即拒绝，refusals 必须为空）。
- 修复后本地：procvar 6/0 共 4 次（含 3 次 `--test-threads=1` 重复）、host 全套 8 个二进制全绿、clippy 0、fmt clean。
