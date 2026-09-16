# 第八批回执 — E1/E2/E3/E4 与 C（续）（980bbc77 续审）

工单：`docs/NEXT_TASKS.md` 第八批；缺陷细节：本目录 `REVIEW.md` E1–E4。
执行日期：2026-09-17。基线：main `39346b45`（审查包入库后）。提交按审查
优先顺序：E1 `c6241318`、E2 `effd09aa`、E3+E4 `7b48f97c`、C（续）
`9d798c44`。执行方式：四个切片并行子代理（文件所有权互不重叠），主代理
逐片独立复验后按序提交；C（续）的 E1 占位场景由主代理按依赖闭包事实
改为装配层移交（见「限制」）。

## E1 — 输出裁剪与投影覆盖（P1）——已关闭（`c6241318`）

- 共享规则 `invalidate_file_read_window_after_body_clip`（agent-workspace
  broker.rs，经 lib.rs re-export）：正文被裁剪且 metadata 声明了窗口
  （start_line/end_line/covers_file）时盖 `window_truncated=true`——既有
  metadata 词汇（artifact.read 已在用），契约零改动；源版本 path/revision
  不动；幂等。broker 的两个裁剪出口（字段上限、总包络上限）与 Runtime
  兜底（agent-runtime output.rs）共用；盖戳后再次过 metadata 预算。
- `truncate_with_marker` 把 marker 本身纳入声明预算（0/1/marker 长度
  边界：marker 放不下时返回其前缀，返回长度永不超过 limit）。
- 组装层零生产改动：盖戳后既有 `file_read_window_from_output` →
  `complete=false` → 跳过不完整窗口的链路自然恢复正确行为。
- 回归（9 条新增）：跨层反例 `broker_clipped_full_read_does_not_hide_…`
  走**真实 WorkspaceOutputBroker**（400 行 ≈34k 字符 > 16k 预算）→
  TurnFrame → PromptAssembler → 最终输入必须含中部 sentinel（红相：旧代
  码把 sentinel 随历史省略，最终输入缺失且无诚实缺失报告）；broker/
  output 各自的盖戳与非窗口对照；未截断路径的正确去重由 4 条既有测试
  钉住不回退。**变异**：注释 broker 出口的盖戳调用 → 跨层测试转红 →
  还原 → 绿。
- 验收：agent-workspace 117/0、agent-runtime 427/0、agent-compose 66/0
  （6 ignored 为既有 live-provider NOT_RUN）；clippy/fmt clean。

## E2 — 参数语义与摘要域一致（P2）——已关闭（`effd09aa`）

- 数值域明确并写入 doc：**整数拼写必须 binary64 无损（|n| ≤ 2^53）**；
  域外整数在参数准入（`SchemaProfile::validate`，Core `execute_published_
  tool` 在审批/派发前调用）以类型化 `SchemaViolation` 拒绝；浮点拼写本
  身即 binary64 值不受影响。授权、摘要、执行从此消费同一语义值（能通过
  校验 ⇔ 规范化字节无损）。
- `minimum/maximum/enum` 约束字面量同域：原 `to_i64` 的 EPSILON 判整与
  f64 往返一并移除（红相实测：`9007199254740993` 字面量编译出的 minimum
  被静默移位成 `…992`）。
- 兼容：JCS 与摘要编码零改动——域内参数（含 1/1.0 等价、键排序、RFC
  8785 整数向量）摘要与旧版逐字节相同；journal/checkpoint 的 digest 是
  不透明 hex、恢复只做结构校验从不重算，历史记录全部可读。不加
  `expect`：域外拒绝是类型化错误，不是 panic。
- 回归：contracts 7 条新测试（2^53 邻域规范化碰撞、负非无损、无约束参
  数同域、约束字面量精确性、边界内放行、机制固定）；Core 链路 2 条
  （域外整数在审批/派发**之前**被拒、边界值正常执行）。**变异**：域检
  查恒 true → 5 条红＋Core 链路红 → 还原 → 绿。
- 验收：agent-contracts 190/0、agent-core 155/0＋12/0＋3 doc、
  agent-platform-protocol 54/0＋15/0；clippy/fmt clean。Core 权限/意图
  检查未动。

## E3 — Linux 分片纳入 agent-host 测试（P2）——已关闭（`7b48f97c`）

- `-p agent-host` 进入 Linux part 1（按 run `35149164719` 实测耗时选较
  轻分片：4m26s vs 5m11s；agent-host 只依赖 `CARGO_BIN_EXE_agent-host`，
  cargo test 自构建）。
- 新增 `scripts/ci_shard_consistency.py`：以 `cargo metadata --no-deps`
  为成员真相，解析 ci.yml 的分片 `cargo_pkgs`，断言 **part1 ∪ part2 ∪
  EXCLUDED == 全部成员**、分片无重复、豁免必须带原因；接入 ubuntu check
  job。本地正向输出 `OK: 20 members = 8 ∪ 12 + 0 exclusions`；负向
  （删 agent-host/无原因豁免/未知包）均 exit 1。
- 如实边界：Linux 分片的实际 `cargo test`（含 agent-host Unix/UDS 用
  例）归 CI；本机验证了脚本逻辑、YAML、与 `cargo test -p agent-host`
  的 Windows 可跑部分（31/0）。

## E4 — mkfifo 测试 CString 修正（P2）——已关闭（`7b48f97c`）

- `runtime_facts.rs` 的 `project_markers_do_not_block_on_a_writerless_fifo`
  改 `CString::new(fifo.as_os_str().as_bytes())`（NUL 终止、分配存活到
  调用后），并新增 FIFO fixture 的 `is_fifo()`＋`S_IFIFO` mode 双重核对，
  之后才跑原不阻塞回归。
- 本机 `cargo check -p agent-workspace --target x86_64-unknown-linux-gnu
  --tests` 通过；**该 Unix 测试在 Windows 上未执行也不能执行**——实际
  运行证据归 Linux CI，不在此冒认。

## C（续）— 固定任务序列的 KV/成本对照（本地阶段）——已关闭（`9d798c44`）

- 新增 `task_sequence_tests.rs`：固定轨迹（同任务/工具契约/provider
  profile/预算）8 连发经**生产装箱→wire mapper→真实 HTTP**到本地捕获服
  务器（127.0.0.1 随机端口），逐步记录稳定策略段/证据段/reuse-boundary
  digest、首差异位置与类别、正文 sentinel、usage 快照。
- 断言结果（真实 digest）：注意力/计数变化 → 稳定前缀与 boundary 不变、
  首差异在证据段之后；新检索/同版本换窗口 → 仅证据区变化、旧边界对新
  请求验证失败；文件修改 → 旧 sentinel 出、新 sentinel 入、边界立即失
  效；工具撤销 → **消息字节全同仅 tools 变，boundary 仍失效**（digest
  绑定工具面——必要失效正确，端点侧能否复用归 T8）；checkpoint 恢复 →
  声明前缀零重写、正文搬移仅在尾部；同态重发 → 捕获 body 逐字节相等；
  `response.failed` 先报 usage 后取消 → 已知计数恰一次入账、分类保持
  Cancelled。
- **变异**：向稳定策略段注入噪声 → 稳定前缀断言红 → 还原 → 绿（证明
  断言真在测稳定段）。
- 验收：`cargo test -p provider-openai --lib` 169/0；clippy/fmt clean。
- 结构性发现（如实记录，未硬修）：`EvidenceSplit` 的 epoch 证据是单条
  SELECTED 消息，任何证据变化整条重写——B0 之上策略段保住但证据段整块
  更换；这是当前装配契约形状，端点侧实际复用归 T8。

## 限制与取舍

- E1：metadata 恰好贴近 8,000 字节上限时，盖戳增量可能触发整体坍缩为
  诚实 truncated 标记（fail closed，无覆盖证明）；未单独造该 ~25 字节
  边界用例。Runtime 兜底在真实链路位于经纪之后（幂等重盖），其独立回
  归以单测覆盖。
- E2：域外整数拒绝是行为变化（新参数不再接受 |n|>2^53）；需要长整数的
  字段必须走版本化字符串 schema——迁移路径已写入 doc，未自动迁移既有
  工具契约。
- C（续）：E1 大文件裁剪维度**不在本 crate 表达**（provider-openai 依赖
  闭包不含 agent-workspace/agent-runtime）：该场景在装配层由 E1 的跨层
  测试承载（真经纪裁剪 → 组装后 sentinel 在最终输入），本 harness 证明
  装配→wire 的逐字节保真——两者合成即「裁剪后的全文读不会变成少发必要
  证据的虚假降本」。原 `#[ignore]` 占位已改为可追溯的移交说明，不再留
  一个永不运行的假 NOT_RUN。测试未驱动 agent-runtime 的 PromptAssembler
  本体（同依赖闭包原因）；端点接受/真实命中/净费用归 T8，NOT_RUN。
- E3/E4：Linux 实际运行（分片 cargo test、fifo 测试、一致性门在 ubuntu
  runner 上）只能由 CI 证明；本机覆盖脚本逻辑、YAML、Windows 可跑子集
  与 linux-target 编译检查。
- 上轮遗留 O1（MetricsSession FullTree 语义）、O2（B2 读取 opened-handle
  硬界）仍开放，维持原编号。
