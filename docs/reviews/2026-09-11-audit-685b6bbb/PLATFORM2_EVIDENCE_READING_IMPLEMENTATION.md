# PLATFORM-2 完整结果与证据读取——实施与验收回执

- 日期：2026-09-11（工作树，基线 `685b6bbb`，未提交）
- 切片：B3 PLATFORM-2（F08 功能缺口）：artifact 有界分页、changes 读模型可定位内容、context 读模型新鲜度
- 三个半的分工与验证全部本地完成

## ① artifact 有界分页（F08 主体，平台线落码＋本回执验收）

四层全通（落码为并行平台会话所做，验收与回归核对在本回执）：

- 协议：`WorkArtifactRequest.offset`（缺省 0＝历史前缀读，兼容旧客户端）＋ `WorkArtifactResponse.{offset, truncated, next_offset, content_base64, size_bytes}`；校验器强制「窗口落在工件范围内」「truncated ⇔ 恰有 next_offset」「非截断窗口必须到达工件末尾」——翻页不可能悄悄换文件或谎报 eof。
- runtime `work.artifact` 路由：seek 定位＋预算窗口（`bounded` 时间预算内）；sealed digest 校验先行。
- 宿主：同一路由透传，e2e 双平台断言。
- .NET：`WorkArtifactResponse` 镜像＋`ReadArtifactAsync(reference, maxBytes, offset, ct)`；`ResumableSession` 经 `RunQueryAsync` 带故障重连。
- 验收：protocol 47、runtime actor work 22、host_e2e 8/8、dotnet 111/111 全绿（本树本日实测）。

**评估**：固定读同一 sealed artifact（digest 校验在每次打开时执行），每页硬上限保留，未把一次读取上限改成无限大——与审查修复要求逐条对应。

## ② changes 读模型：可定位的内容引用（本切片实现）

审查原文：「能从变化摘要定位实际内容」。基线 journal 捕获了 `old_content` 但协议层刻意不上 wire——GUI 只见 hash，看不到内容也没有取回路径。

**修**：`ChangeSummary::MutationPrepared` 增 `old_content_artifact: Option<String>`（serde 缺省省略）。runtime `work.changes` 路由把捕获的 before-body **一次性溢出**到 run 的 sealed artifact store（内容寻址：相同内容自然去重），回执只给引用；GUI 用 ① 的分页路由读原文——changes → artifact.read 形成「定位→补读」闭环。溢出失败降级为 `None`（行仍可经 hash 审阅），绝不让读日志整体失败。协议校验：引用存在时按 opaque＋`MAX_ARTIFACT_REFERENCE_BYTES` 校验。捕获上限（256 KiB）与页行数上限不变，无新无界面。

## ③ context 读模型：新鲜度与驻留（本切片实现）

审查原文：「看到信息的新鲜度与完整性」。`ContextItemSummary` 增两个 serde-default 字段：

- `residency`（`Resident`/`Warm`/`Cold`/`External`，缺省 Resident）：正文此刻在哪里——驻留工作集、可逆缓冲，还是存储（无 fetch 只见摘要指针）。
- `selected_current_turn`：正文是否进入了**当前回合最近一次材料化的模型表面**——「本轮实际发送」，而非仅仅驻留。

context-simple `inspect` 投影：heap/buffer/retry 条目带各自权威 residency；外部条目按定义 `External`；`selected_current_turn` 由 `last_selected_turn == 当前回合`（turn 0 基线除外）统一盖章。context-baselines 投影按其内存语义填 `Resident`＋从不声称「已发送」。.NET `ContextItemSummary` 镜像两字段（可空容忍旧服务端）。

## 回归（3 项新增）

| 测试 | 覆盖 |
|---|---|
| `agent-runtime tests/actor/work_control.rs::changes_rows_locate_captured_content_through_an_artifact_reference` | journal 记录 → changes 响应带引用 → work.artifact 分页读回原文逐字节一致；`!truncated`＋无 next_offset |
| `context-simple tests/lifecycle.rs::inspect_reports_residency_and_actual_send_freshness` | 外部条目 `External`＋未发送；materialize 后发送条目 `Resident`＋`selected_current_turn` |
| 协议 fixture/构造点机械补齐（`old_content_artifact: None`） | 旧形状兼容 |

## 实际检查（全部本地执行）

- `cargo test -p agent-platform-protocol`：47＋10 全绿
- `cargo test -p agent-runtime --test actor`：**81 通过**（含 PLATFORM-1 的 22 项与新回归）
- `cargo test -p context-simple --lib`：**321 通过**（基线 320＋1）
- `cargo test -p agent-host`：lib 8＋host_config 3＋host_e2e **8/8**＋restore 3 全绿
- `dotnet test clients/dotnet/Agent.Client.Tests`：**111/111**
- `cargo check --workspace --all-targets` 0 警告；`cargo fmt --all -- --check` 通过；`cargo clippy -p agent-platform-protocol -p agent-runtime --all-targets` 0 警告
- `python scripts/doc_consistency.py`：OK

## 未验收（如实记录）

- 未提交/推送、未跑远端 CI；未在真实宿主＋真实大工件上人工走查分页（host e2e 与协议金样为当前证据）。
- 共享树与并行平台会话的活跃编辑窗口重叠，一次编译失败与一次 host_e2e 失败经重跑确认为瞬态中间态，最终全绿以本回执记录为准。

## 下一步

C2 GUI-2 消费本切片（分页继续阅读/eof/引用）；随后 B4/GUI-4 第三波。
