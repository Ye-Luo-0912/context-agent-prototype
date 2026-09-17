# KV 回执 — 有值缓存桶、失败/重试累计、实际维护 lane

**提交：`a31e9434`**（第十一批，基线 `d3a05d29`，审查：[REVIEW.md](REVIEW.md)）。唯一改动文件 `crates/agent-compose/tests/kv_production_sequence.rs`（+1213/−0，纯增量）；现有 26 轮轨迹与全部既有断言一字未改。测试 only，生产代码零改动。

## 三个新测试（全部真实 Compose/Actor/Provider＋本地 127.0.0.1 捕获服务器）

### a) `synthetic_cache_buckets_settle_per_round_without_invented_zeros` — 有值缓存桶
4 轮轨迹，`response.completed` usage 携带 **LOCAL SYNTHETIC** 缓存计数：R1 cached=300/write=40/miss=3801（hit+miss=input）；R2 有读有缺 miss 无 write；R3 全缺测；R4 有读有写无 miss。断言：每行 typed 三桶（`CostCounter::Known/Unknown`）逐字携带合成值；缺测桶保持 Unknown（不补零、不派生 input−cached）；typed 桶与 event 级 flatten 三方一致（缺测时 flatten=0 且 typed=Unknown）；hit+miss==input 可复算；轨迹总额＝各行之和＝各 served 之和；attempts=1/retries=0/role=Main/Observed。

### b) `billed_retryable_failure_and_its_retry_settle_exactly_once` — 失败/重试累计
生产装配外包 `RetryingTransport::new(provider, 2, 1ms)`（与 compose 组合根同型）；T1 第 1 次尝试为 `response.failed`（server_error，可重试，带已知 usage，无 text delta 避开 LiveSink 重放屏障）→ 生产重试成功；T2 干净轮。断言：3 次线上请求、2 轮账（重试不是第二轮）；合并行 input=4201+4202、output=7+8（失败尝试成本不抹、不重复计）；cached 合并 300+300=600、失败侧 miss=3901 逐字保留（R2/QB 语义）、write 缺测保持 Unknown；attempts=2/retries=1；观察到真实 `ModelRetrying(attempt=2)` 事件；干净轮 attempts=1/retries=0 不泄漏；账本总输入＝全部尝试之和（含失败那次，恰好各一次）。注明 hit+miss==input 只按次成立、跨尝试合并不回填。

### c) `maintenance_lane_compaction_settles_on_its_own_lane` — 实际维护调用（本地可达，已真实触发）
触发方式：`build_context_engine(ContextPolicy::Rolling, …, Some(model), None, &MaintenanceBudget::default(), Some(routing.key_for("compaction","maintenance")))` 挂生产 `ModelBackedCompactor`（与 host/TUI 组合根同一条 lane-key 派生）；经 engine 真实 ingest 预置约 40 条记录跨过 rolling 折叠阈值（与 `cache_routing_wire_acceptance` 同一 seam）；折叠在 actor 的**回合内维护路径**（turn-start/turn-tail `spawn_maintenance` → `context.maintain` → compactor HTTP）真实发起，本次运行触发 3 次维护调用（诊断行 `KV_SEQ_MAINTENANCE maintenance_calls=3 maintenance_input_total=12903 main_input_total=8623`）。断言：维护请求带维护 lane routing key、无 tools、压缩 system prompt、`prompt_cache_options.mode=explicit`、`max_output_tokens=512`；每次调用恰好结算一条 `ContextCompacted`（Reason=RollingFold、Observed、三桶与 attempts/retries=1/0 逐字一致、split 可复算）；主 lane 严格分离——所有 `ModelUsed` 行 role=Main、无维护 token 身份混入、主账本总和＝主轮次之和，维护用量只在自己那侧入账。

## 命令与结果（实际执行）

- `cargo test -p agent-compose --test kv_production_sequence`：**4 passed / 0 failed**（现有轨迹＋3 新测试），并行 14.1s、串行 18.5–18.6s，共 3 次全绿无 flake；集成人复跑 14.04s 同结果。
- `cargo fmt -p agent-compose -- --check`：通过。集成终验 `cargo test -p agent-compose` 全套 0 失败。

## 诚实边界与剩余限制

- 所有缓存/尝试计数均为 **LOCAL SYNTHETIC**（脚本化捕获服务器），只证明传输＋解析＋结算记账；**ENDPOINT_ACCEPTED / SERVER_HIT / NET_TASK_COST 仍为 NOT_RUN**（归 T8，未联真实付费端点）。供应商缓存要求精确前缀匹配且工具契约参与其中；本轨迹不冒充命中或净费用收益。
- (c) 的折叠阈值由测试经 engine ingest 预置跨越（生产默认 9k token 阈值下短轨迹不会自然跨过）；触发本身走生产回合内维护路径，与既有验收同 seam。
- 每次 fold 的调用次数由 rolling 引擎自身 cadence 决定（本次 3 次），测试按集合精确匹配断言、对次数不敏感。
- 语义正确性是缓存对照前提：H1/H2 落地后，本轨迹的语义完整性断言不受「错误终结旧约束／冷页失效未生效」污染。
