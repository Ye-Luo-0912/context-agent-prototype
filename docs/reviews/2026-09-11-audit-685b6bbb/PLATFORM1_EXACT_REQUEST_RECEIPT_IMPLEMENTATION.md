# PLATFORM-1（F06）实施回执：精确提交结果查询

基线 `685b6bbb29275bc8ec73ce6625a94567a8b8d23d`（2026-09-10 提交、CI run `34513313166`）。本切片在**工作树**落地（未提交、未推送、未跑远端 CI）。对应 [REPORT.md](REPORT.md) F06（P1）：未知提交按 Goal 文字核对、缺少请求身份依据；对应 [NEXT_STAGE_THREE_TRACKS.md](NEXT_STAGE_THREE_TRACKS.md) PLATFORM-1。

## 用户能获得什么

断线、刷新或宿主重启后，客户端可以凭**本次提交自己的 `client_request_id`** 向运行时账本做一次只读查询，得到分型事实：已受理（含任务绑定）、同 id 异内容的明确冲突、或无证据（unknown）。不再用「快照里有没有同名目标」把未证明状态说成确定结论；unknown 永远不自动变成「未执行」，也永远不自动重发。

## 实现落点（按层）

**Runtime 账本（`crates/agent-runtime/src/work.rs`、`command.rs`、`actor/commands.rs`、`lib.rs`）**

- `WorkSubmissionRecord` 增加 `payload_digest`：受理时按协议侧同一 domain 分离 SHA-256 计算。submit 载荷本就只有 `{goal, client_request_id}`（`WorkSubmitRequest`），goal 即完整载荷内容，id 是键——digest 覆盖 goal 是完整载荷身份，不是把 Goal 当幂等键（幂等键始终是 `client_request_id`）。
- 新增 `WorkSubmissionQuery`：`Recorded { task_id, payload_digest, matches }`（`matches` 为调用方自报 digest 与账本记录的比较，`None` 表示调用方未报、只证身份）与 `Unknown`（无证据，双向都不证明）。经 `RuntimeCommand::QueryWorkSubmission` / `RuntimeHandle::query_work_submission` 暴露；actor 处理与 `TaskDetail` 同类——只读，无 idle fence、无模型轮、无 checkpoint。
- 账本仍是有界进程生命周期 `VecDeque`（`MAX_PENDING_WORK_SUBMISSIONS = 256`），刻意不进 checkpoint：重启后的运行时把旧 id 视为 unknown，调用方必须查询而不是假设 exactly-once。

**协议（`crates/agent-platform-protocol/src/work.rs`）**

- 新只读路由 `work/submit_result`；`WorkSubmitResultRequest { client_request_id, payload_digest? }`（opaque 上限 128 字节，digest 可选）。
- `WorkSubmitResultDisposition` 五分型：`accepted` / `already_accepted` / `known_rejected` / `unknown` / `expired`（snake_case wire），附 `is_admitted` / `is_indeterminate` 助手。
- `WorkSubmitResultResponse` 绑定 `run_id`（回答来自哪个 run，重连到别的 run 必须重查）并回显被查询的 `client_request_id`（迟到响应不能张冠李戴）。校验规则：recorded 分型必须带非 nil `task_id`；indeterminate 分型禁止带 `task_id`；`accepted_payload_digest` 只允许出现在 `known_rejected`；成功回答的 id 必须等于被问的 id。
- `submission_payload_digest`：domain 分离 SHA-256（`SUBMIT_PAYLOAD_DIGEST_DOMAIN = "focus-agent.platform.work.submit-payload.v1"`，preimage = `DOMAIN || 0x00 || utf8(goal)`，小写 hex），Rust 与 .NET 从同一常量/同一文档 preimage 各自实现。

**Runtime 平台路由（`crates/agent-runtime/src/platform/work.rs`）**

- 新授权动作 `ReadSubmitResult`（operator 与 read_only 会话都允许——纯观察类），`submit_result` 路由走既有 bounded/validate/error 管线。分型映射：账本 `Recorded` 且调用方 digest 匹配（或未报 digest）→ `Accepted`（附任务）；`Recorded` 且 digest 不匹配 → `KnownRejected`（附任务与原受理 digest，终态：该 id 换内容永不再受理）；账本无该 id → `Unknown`。
- 设计说明：runtime **不产出** `already_accepted` 与 `expired`。`Accepted` 的语义是「本 run 受理过该 id」（不区分首受理与幂等重读，原受理即权威）；`expired` 是为「持久化收据」这一**尚未作出的产品承诺**预留的 wire 分型——在进程内账本的事实模型里，无证据就是 `Unknown`，runtime 不冒装知道「曾经见过但已淘汰」。跨重启确认若将来成为承诺，必须按任务书与提交/检查点权威一致地设计 accepted 提交点与恢复判定，不是扩容 HashMap。

**宿主（`crates/agent-host/src/lib.rs`）**

- dispatch 增加 `("work", "submit_result")` 分支，走与 B3 四条只读路由相同的 run-scoped 管线。

**.NET SDK（`clients/dotnet/Agent.Client/{Envelope,WorkDto,IAgentConnection,AgentConnection,ResumableSession}.cs`）**

- `Route.WorkSubmitResultRoute()`；`WorkSubmitResultRequest`/`WorkSubmitResultResponse` 逐字段镜像 wire（deny-unknown、null 抑制、validate 规则与 Rust 一致，含事实绑定矛盾即抛 `AgentContractViolationException`）。
- `SubmitPayloadDigest.Compute`：与 Rust 字节一致（domain 常量 + 0x00 + UTF-8 goal，SHA-256 小写 hex）。
- `IAgentConnection.SubmitResultAsync(clientRequestId, payloadDigest?)`；`AgentConnection` 直发；`ResumableSession` 经既有 `RunQueryAsync`（故障重连一次后重问）。分型注释明确 `Unknown`/`Expired` 是证据事实，永远不是重发许可。

**GUI 消费半（`apps/Agent.Desktop`）**：归 GUI 线（C3/GUI-3）所有。本切片交付时该线正在同一工作树并行接线（快照 Goal 匹配改为账本查询），其代码与测试不在本回执的验证范围内。

## 回归（先红后绿与对照）

Rust（新增测试函数 3＋4，e2e 为既有用例内新增断言三臂）：

- 协议 `submit_result_query_binds_the_answer_to_the_asked_request`（回答必须关于被问 id；身份查询合法；三种分型的事实力一致）、`submission_payload_digest_is_stable_and_content_bound`（金样 preimage、大小写敏感、64 位 hex）、`submit_result_disposition_and_task_facts_must_agree`（recorded 无任务 / indeterminate 带任务 / 非 conflict 带 digest 均拒绝；超限输入由既有 fail-closed 校验覆盖）。
- runtime actor：`unknown_request_is_unknown_even_when_a_same_goal_task_exists`（**报告反例**：同 Goal 旧任务在列，未受理 id 仍是 Unknown）、`recorded_request_reports_its_task_and_matches_the_named_payload`（受理＋digest 匹配/冲突两臂）、`query_is_read_only_and_does_not_execute`（查询不执行、不改状态）、`evicted_receipt_becomes_unknown_not_a_negative_proof`（把 256 条账本挤过期后，原 id 读作 Unknown 而非「未执行」；原任务身份不受影响）。
- host e2e（named pipe＋UDS 各跑一遍 `run_e2e`）：wire 层三臂——已受理 id＋正确 digest → `Accepted`＋任务绑定＋run_id 回显；同 id 异 digest → `KnownRejected`＋原受理 digest；未见 id → `Unknown`（`is_indeterminate`，无任务）。

.NET（`Agent.Client.Tests`）：

- `Submit_result_disposition_pins_snake_case_wire_and_fact_binding`：wire 形状钉死（snake_case 分型、deny-unknown、字节级 roundtrip）；indeterminate 带任务、非 conflict 带 digest、未知字段均拒绝。
- `Submit_payload_digest_matches_the_host_golden_preimage`：跨语言金样 `f9e5a123a1495f7c02973725850272a97e070a586e6640b570dd06db44170016`（= Rust `submission_payload_digest("migrate the retry table")`，sha256sum 独立复核）。
- `Real_host_answers_the_exact_submission_receipt_query`：真实 `agent-host` 二进制（`AGENT_DEMO=1`）＋生产 `ResumableSession`——真实受理后查询得 `Accepted`（任务绑定、run_id、id 回显）；同 id 异内容得 `KnownRejected`（报告原受理 digest）；**换 id 但 Goal 文字相同**仍得 `Unknown`（Goal 不是幂等键的直接证据）。宿主重启臂由 runtime actor 的 eviction/restart 语义与 C3 走查承接，本测试不重复。

## 实际执行的检查（本机 Windows，2026-09-11）

- `cargo test -p agent-platform-protocol --lib`：**45 通过**（B3 基线 42＋本切片新增 3）。
- `cargo test -p agent-platform-protocol --test work_fixtures`：10 通过。
- `cargo test -p agent-runtime --test actor work`：**22 通过**（含新增 4）。
- `cargo test -p agent-host --test host_e2e`：**8/8 通过**（含 submit_result 三臂 wire 断言）。
- `cargo fmt -p agent-platform-protocol -p agent-runtime -p agent-host -- --check`：通过。
- `cargo clippy -p agent-platform-protocol -p agent-runtime -p agent-host --all-targets`：0 警告。
- `cargo check --workspace --all-targets`：通过（`tool-runtime` lib test 的 3 个 unused-variable 警告来自并行核心线在飞的 search coverage 改动，不属本切片文件）。
- `dotnet test clients/dotnet/Agent.Client.Tests` 过滤 `FixtureConformanceTests|HostChainTests`：**21/21 通过**（含新增 3 项；真实宿主测试实际运行，非 NOT_RUN）。
- 全量 dotnet 套件在最终运行时为 **101/101**（验证中途曾观察到 99/101：2 个失败在 `RestoreWalkthroughTests`，属 GUI 线 C3 旧「按快照解除」行为断言的同步改写；该线完成改写后全绿——平台查询面与 GUI 消费半在同一共享树上会合）。

## 未验收 / 边界（如实记录）

- 未提交、未推送、未跑远端 CI；「关闭」待 CI run 记录确认。
- **跨重启确认未实现，且按任务书明确不实现**：持久化收据是产品承诺，需要与提交/检查点权威一致的 accepted 提交点与恢复判定设计；本切片只把 256 条进程内窗口与重启边界如实暴露为 `Unknown`。
- `expired` 分型已进 wire 契约但 runtime 不产出（无证据时不冒装知道历史）；`already_accepted` 在查询答案里并入 `Accepted` 语义（原受理即权威）。
- GUI 消费半（删除按 Goal 核对的界面逻辑、unknown/已受理分状态文案）由 GUI 线 C3 承接，本回执不验证其代码与测试。
- 真实 provider 场景照旧 NOT_RUN（本切片为平台事实层，不依赖 provider）。
- 共享工作树同期有并行线改动落进 `agent-core`/`tool-runtime`/桌面 ViewModel；本切片的 Rust 检查按 crate 划界执行，未对全仓 fmt/clippy 状态背书。
