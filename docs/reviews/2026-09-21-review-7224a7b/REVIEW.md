# V5 耐久测试根因审查

审查对象：`Ye-Luo-0912/context-agent-prototype`

分支：`codex/runtime-endurance-full-plan`

固定源码提交：`7224a7baa8910881b9efaa77f14013781137ff49`

本报告是**固定提交的测试根因审查和相关 Runtime 链路审查**，不是“全仓逐行阅读、全工作区测试通过”的认证。结论分为源码确认、本地反例确认、原回执报告、待原始轨迹确认四类。没有修改远端仓库，没有调用付费供应商。

## 1. 结论

V5 未通过验收，但不能据此直接判断模型能力、Context/GC 或模型预算是主要原因。源码确认至少有以下干扰：

1. 模型受保护的 SPEC 要求 immutable 读取，而修改后的 oracle 使用 mode=ro 读取 WAL。
2. 规定最多 256 个归档成员，但停止后的数据有 357 个 objects；按现有布局，加上 91 个 manifests、13 个 current 指针、descriptor 和 1 个 outbox，至少需要 463 个成员。
3. 所谓持续并发负载是串行子进程循环，而且任何早期失败都会永久阻止同一次负载最终 PASS。
4. oracle 在四个零供应商反例中放过了应当拒绝的状态。
5. 长任务仍沿用 48 次 Python 执行、30 分钟授权的短测试默认参数。
6. 任务相关、持续变化的反馈文件可以被 Runtime 算作新的前沿证据，而不代表实现或验收取得进展。

因此，原始 `NOT_ACCEPTED_MODEL_BUDGET_EXHAUSTED` 回执应原样保留；审查另记“测试契约/控制器/判定器问题导致模型归因不充分”，不能反向把本轮改成通过，也不能靠继续追加预算覆盖本轮。

## 2. 版本与覆盖边界

### 2.1 版本身份

- 本次分支查询返回 `7224a7b`，提交时间 2026-09-20T16:56:41Z，消息为 `runtime: complete endurance workflows and V5 long-run campaign`。
- V5 的 `FINAL_STATUS.json` 记录的基线 HEAD 是父提交 `71322074596b7e604be5c8750d350e7d626b0e28`。不能只凭这个 HEAD 证明测试二进制等于后来提交；还需当时的 dirty patch、二进制哈希及配置摘要。
- 对 `7224a7b` 的 GitHub Actions 查询返回 0 个 workflow run。当前工作流仅在 push main 或 pull_request 时触发。
- 前面读取的 `8764b94` 属于旧批次，不用于证明本次 V5 的代码行为或测试结果。

### 2.2 本轮实际覆盖

| 范围 | 实际工作 |
|---|---|
| 仓库结构 | 读取根目录、20 个 Rust crate 的目录清单、关键子目录和证据目录 |
| V5 | 全文读取 SPEC.md、run.py、prepare.py、preflight.py、continuous_load.py、oracle.py、invoke.py、finalize.py |
| V5 证据 | 全文读取 PLAN、RUN、FINAL_STATUS、MANIFEST |
| 共享 runner | 读取配置、估价、账本、授权、进程清理、run_segment、汇总等关键区段；并非全文 |
| Runtime | 全文读取 execution/classify.rs、execution/freshness.rs；定向读取 execution/state.rs、actor/tools.rs |
| CI | 读取工作流，查询固定提交实际运行记录 |
| 本地执行 | 执行精确 Git blob 对应 oracle 的 4 个合成反例和 WAL 可见性微实验 |
| 未完成 | 全部 20 个 crate 源码、所有测试和 .NET GUI 的逐行审查；Rust/.NET 编译及全工作区测试；真实供应商重跑；候选模型源码及原始请求轨迹复盘 |

本地环境没有可用 cargo/dotnet；Git 克隆网络路径失败后使用已连接 GitHub 工具读取固定提交。上述限制不影响已经取得的源码反例，但不能扩展成全仓通过或全仓完整审查的声明。

## 3. 原回执报告了什么

来源：`docs/experiments/package-endurance-v5/RUN_2026-09-21.md` 与 `final-evidence-final-r2/FINAL_STATUS.json`。

| 项目 | 原回执 |
|---|---|
| 第一模型段 | 41 个模型轮、76 次 tool_finished；model_output_limit；报告当时未创建候选模块 |
| 第二模型段 | 219 个模型轮、319 次 tool_finished；TurnCompleted 后 session 为 approval_denied |
| 主决策合计 | 260 |
| 供应商 attempts | 262 |
| TaskId | 仅一个：f9b1e916-3575-40e8-b7c5-c2ff1a73ecf9 |
| 任务完成 | false；awaiting_operator_review |
| 估算金额 | USD 1.498222068；reserved=0、unknown=0；不是供应商账单 |
| 受保护文件 | 496，回执称未改变 |
| 候选测试 | 27 项，9 项 errors，退出码 1 |
| 备份/恢复 | 0 个通过独立 oracle 的 cycle；未达到同一候选版本 120 分钟负载 |
| 最终源数据 | 13 scopes、218 receipts、91 manifests、357 objects、1 outbox |

这些是归档摘要，不等于本轮独立重跑。尤其是 `TurnCompleted` 不能解释成任务验收通过，第一段的 `model_output_limit` 也不能解释成上下文窗口耗尽。

## 4. 按原因展开

### F01 — 受保护规范与 WAL 判定不一致【源码确认；阻断下一次实验】

位置：`scripts/package_endurance_v5/SPEC.md` 的 Online cut、`prepare.py::prepare`、`oracle.py::_connection`。

SPEC 明确要求 source 使用 percent-encoded **immutable read-only URI**；prepare 把这份 SPEC 复制到模型工作区并纳入保护哈希。运行提示又禁止模型改 SPEC、原应用、snapshot 和判定器。

但 oracle 的读取方式是 `?mode=ro`，注释明确说为了纳入 committed WAL 而不用 immutable。原 RUN 回执也承认 oracle 调整过，并把候选 immutable/拒绝 sidecar 作为失败项。

这造成了模型的规范权威与实际裁判不一致。不能把“遵循 immutable 要求”本身作为纯模型错误。由于公开证据没有包含当时 workspace/SPEC 的原始字节及 baseline-lock，精确的 campaign 身份还需原件复核；当前源码中的矛盾及 prepare 的复制路径则已确认。

本地 WAL 微实验保留写连接并关闭自动 checkpoint 后，mode=ro 读到 4 条已提交 receipt，mode=ro&immutable=1 读到主文件内的 3 条。这个结果只证明本地版本下的可见性差异，不是运行了缺失的候选代码。

SQLite 官方把 immutable 定义为数据库不会变化的承诺，并明确它跳过锁和变化检测；不能把它理解为普通的只读访问选项。

**修复要求**：先冻结一个自洽合同：究竟是静止源、调用期间允许写入的在线一致性切点，还是在受控协调下取得快照。规定 WAL 的读取与 sidecar 权限、允许的有界拒绝及恢复义务。修改合同或 oracle 后另立证据身份，不覆盖 V5 原始失败。

### F02 — 工作负载与归档容量不相容【源码＋原回执计算】

位置：`SPEC.md`、`prepare.py`、`FINAL_STATUS.json`。

SPEC 要求完整保留历史对象，同时规定归档最多 256 个成员。prepare 生成 120 个包、每包 4 个版本的资源池。最终摘要的 357 个 objects 已单独超过上限；当前每对象/manifest 独立成员的布局至少需要：

```
357 objects + 91 manifests + 13 pointers + 1 descriptor + 1 outbox = 463
```

尚未计入 journal 成员。因此，至少在该最终规模下，完整备份与 256 成员上限不能同时满足。候选自己设置的 receipt 上限是否错误仍需候选源码，但“删除所有 bound 就能修好”不是可靠结论。

**修复要求**：运行前计算最大引用闭包及成员/字节需求。要么修改格式/分片策略，要么正式调整资源上限，要么缩小冻结负载；不得让模型自行违背受保护规范。

### F03 — 负载循环不是计划中的真实并发【源码确认】

位置：`continuous_load.py::run`、`candidate_process`。

实际是单个 for 循环，install/active、backup、restore 均等待子进程完成再继续。不存在四个同时工作的 install worker，也没有计划中的 GC worker。Receiver 使用线程并不等于仓库写者与备份并发。

`random.Random(seed)` 创建后未用于调度；不能从 seed 参数存在推断已经完成确定性故障编排。唯一 index=5 的 crash 调用只记返回值，未强制断言 exit=74，也未接上要求的正常重试。原摘要实际记录的是 returncode=0、ValueError(receipt bound)，因此该记录不是 before_publish 强杀已触发的证据。

**修复要求**：为写者、读取/GC、备份校验建立真实独立执行者和可观察的重叠区间；故障使用 planned/fired/observed/verdict 账目，触发条件未成立保持 NOT_TRIGGERED。

### F04 — 早期失败使修复后的持续任务无法 PASS【源码确认】

位置：`continuous_load.py::run`。

控制器在候选文件还不存在时就尝试 backup_live。错误累计到 failed；后续不因候选更换而分开计算。最终条件是：

```python
status = "PASS" if archives and failed == 0 else "INCOMPLETE"
```

因此第一次没有实现/普通开发错误就让这一整个持续运行永远无法 PASS。计划允许同一任务阅读反馈并修复，判定却要求从第一轮开发起零失败；两者不一致。

同时，控制器没有冻结候选源码集合、没有按候选版本建立 120 分钟计时。后续刚修改的代码和先前代码的观测可能混在一起。

**修复要求**：历史失败永不删除；另外维护按 candidate_digest 绑定的 acceptance window。只有最后冻结版本的窗口必须满足规定时长、通过率和故障覆盖。替换版本关闭旧窗口并开启新窗口，但不重置任务身份、总预算或历史证据。

### F05 — 模型段与负载寿命错误耦合，resume 缺少负载状态恢复【源码确认】

位置：`run.py::run`、`continuous_load.py::run`。

run_segment 返回后，无论模型失败、被拒还是普通结束，finally 都 terminate loader。没有 campaign 层协调“模型停止生成后，已冻结候选仍执行无供应商调用的剩余负载”。这个设计也不能正确表达有界等待外部事件。

传入旧 repository 做 resume 时，generation_by_scope 仍从空字典开始，index 也从 1 开始；没有从旧数据库恢复各 scope generation 与已使用请求序号。确定的后果是可能提交旧 expected_generation 和旧 release key；实际本次产生了多少此类拒绝/幂等复用，须查看完整事件。

**修复要求**：campaign 拥有写者、Receiver、负载和模型执行者的寿命。恢复时保留同库身份、请求计数、generation 及发布身份；预算耗尽时停止新付费请求，但是否继续纯本地候选验证应由原冻结合同决定，不能临时改变。

### F06 — oracle 有四个可复现漏检【本地反例确认】

位置：`oracle.py::repository_cut`、`check_restored`、`check_archive`。

本地 oracle 文件的 Git blob SHA 与固定提交一致：

```
b459e27152c6b2ea8cb8416ce25cc5002eebe3ab
```

| 反例 | 预期 | 实际 |
|---|---|---|
| 保留 generation 1、3，删除非 current 的 generation 2 | 拒绝历史缺口 | 接受 |
| 数据库和磁盘 current 一起回退到 generation 1，历史仍有 generation 3 | 拒绝非最新 current | 接受 |
| 在应与 cut 对应的恢复目录加入额外文件 | oracle 发现不匹配目录 | 接受 |
| 构造 263 个成员、92,134 bytes 的规范 ZIP | 拒绝超过 256 成员 | 接受 |

每类反例在修改前先验证其合成基线被 oracle 接受。它们是 oracle 输入面的最小合成反例，**不是完整 V3 应用、模型候选或 Runtime 的端到端运行**。不可把这些结论改写成“候选 restore_live 接受了恶意输入”；被测对象是控制器的 oracle。

运行脚本见 `probes/probe_v5_oracle.py`；输出见 `probe_results.json`。退出码 1 表示发现 oracle 不变量漏检，是红证据，不是通过的修复回归。

**修复要求**：补齐 generation 连续性/最大性、完整目标 byte map、成员/展开字节数量限制。继续补 outbox 丢失、重复发布身份、跨 scope、journal 资源闭包等反例；不要只保留三个容易拒绝的校准样本。

### F07 — oracle 本身没有建立真实在线一致性切点【源码确认；并发影响待回放】

repository_cut 连续执行多个 SELECT，没有显式读事务；关闭数据库连接之后，才读取 manifest、object 和 current.json。控制器又在候选备份返回后读取源库来构造 expected_members。

只把 immutable 改成 mode=ro 不会使多个 SQL 查询和文件读取成为原子快照。真正并发时可能混合不同提交时刻；即使候选取得调用区间内合法切点，也可能因 oracle 使用较新的源状态而被误拒。

Python sqlite3 的 legacy 事务控制不会为这些 SELECT 自动建立一个覆盖后续所有查询的事务。SQLite Online Backup API 可用于数据库快照，但数据库外的对象、指针和 GC 仍需独立的一致性/保留协议。

**修复要求**：明确 cut identity 与调用区间，保留可复核的提交证据；数据库侧使用明确的快照方案，文件侧依据 cut 建立引用保护或验证重试。oracle 不能拿“调用返回后的最新状态”等同于唯一正确答案。

### F08 — 关键失败被后续普通反馈覆盖【源码确认；实际模型暴露待轨迹】

位置：`continuous_load.py` 写 `runtime-feedback/latest.json`。

每个循环都覆盖 latest，包括没有 backup 的普通 install 批次。因此偶数轮的 WAL/receipt 失败，很快会被奇数轮的 install 成功详情替代，虽然 failed 总计仍在。提示又要求模型主要阅读 latest.json。

这会令“失败数量”可见而“当前应修的失败、触发输入、候选版本、复现命令”不持续可见。不能从这里断言模型在本轮一定读到了错误版本；需要逐轮请求与文件摘要。

**修复要求**：保留有界 unresolved_failures 列表。只有同一义务的匹配成功或明确失效条件才能解除；普通批次推进不能覆盖未解决失败。

### F09 — 长流程仍使用短测试授权【源码确认；具体拒绝原因未证实】

位置：`runtime_endurance_incremental_runner.py::_prepare_segment_files`、`package_endurance_v5/run.py`。

- Python 进程授权 max_runs=48。
- 写入及进程授权 expires_at_ms=当前时间+1800秒。
- V5 没有覆盖这套设置，却要求 260 决策、持续修复和两小时最终负载。

第二模型段以 approval_denied 结束。这与授权用尽相容，但没有原始拒绝事件，不能说已经证明恰好是第 49 次 Python 执行；argv 不匹配等其他原因仍可能成立。

**修复要求**：在开跑前检查任务动作需求与授权预算是否相容。授权保持有界、具体到合法执行参数和路径；合法续租绑定原 campaign 总额度。禁止以全局自动批准、清除 Core 围栏等方式掩盖问题。

### F10 — 金额预约元数据和真实算法不一致【源码确认；没有证明本轮超支】

位置：V5Ledger._reserve_policy、BudgetLedger.reserve、Pricing.reserve_estimate_usd。

V5 记录的策略名是 wire_bytes_plus_8192_and_full_output，但真实金额预约仍调用继承的 Pricing 方法：

```
输入估计 = request_body_bytes / 4 + 256
输出估计 = max_output_tokens
```

V5 的输入 token 总额前置检查使用 body_len+8192，不等于金额预约也使用它。说明文字覆盖并没有改变 reserve 的实际算法。

此外，main_decisions 只在 V5 的每次 run 调用上校验 rounds<=260，没有从以前段的累计决策里机械计算余量；tool_attempts=900 也未在所读 V5 受理路径形成同一 campaign 的强制计数。此次 41+219 的回执确实没有超过260，但不证明跨段上限由代码自动保证。

**修复要求**：一份实际执行的预约函数同时产生说明和计算；区分主模型决策、网络重试、工具受理、金额和 token 五个计数，跨段共享并拒绝超额。历史 $1.498 仍只作为原回执估算，不改写为超支。

### F11 — 归档是单次汇总，不是完整可重演证据包【源码确认】

位置：`finalize.py`、`final-evidence-final-r2/MANIFEST.json`。

finalize 的总体 status、候选测试退出码、固定两个模型段、四个 controller 输出名称及若干结论为硬编码；model-tests.log 从固定 r3 路径读取，而不是函数 stage 参数。MANIFEST 实际只校验 FINAL_STATUS.json。

这不证明原回执数字是虚构的，但意味着该脚本不能作为通用、独立的验收裁决器。当前公开 V5 包没有候选代码、全部请求/工具事件、授权拒绝详情、候选版本切换轨迹以及完整运行身份材料，无法从一个摘要重建因果链。

**修复要求**：完整证据包至少冻结：source SHA + dirty patch + binary hash、实际 SPEC/controller/oracle/config 哈希、候选源码集和测试、原始事件/请求、版本绑定的反馈、预算与授权账本、逐类清理确认。结果生成器从材料派生，不把结果写死。

## 5. Runtime 主体：已有机制与具体改进点

### 5.1 不应重开为“完全缺失”的机制

固定源码已包含：

- 重复行为阈值 3、同类失败跨目标聚类、前沿无推进阈值 5。
- 每轮重新投影当前 stall/frontier warning，而非提示一次就消失。
- 区分新证据、重复、重新确认与因本地容量而不可追踪的窗口。
- 新发现但与已知任务根无关的证据，不应持续重置任务前沿。
- 读取动机分类：Changed、Warm、Stored、ProtocolCheckpointBodyMissing、NeedsRevalidation、BodyVisibleCurrent 等。

原摘要也表明 TaskId 没有被两个模型段重建。这些是应该保留的基础，不能重新写成“没有 Task/上下文/防重复机制”。

### 5.2 反馈新鲜度可误充任务进展【代码链路确认；V5 因果未实证】

链路如下：

```
prompt 明确提及 runtime-feedback/latest.json
→ actor/tools.rs 将当前指令精确提及的路径视作 rooted target
→ 每批次 feedback 的 batch/elapsed 等字段变化
→ fs.read 获得新的资源摘要/证据
→ freshness.rs 可分类 EvidenceAdvanced
→ update_convergence 重置前沿停滞计数
```

这说明当前前沿仍可能把“又读到了新的监控快照”当作“任务朝验收推进”。最新反馈是相关文件，因此已有排除无关探索的规则不能处理此情形。

这不是说所有新的观测都不算进展，也不是证明 V5 的219轮实际全部在此处循环。它是一个可构造、应增加回归的 liveness 盲点。

**建议**：在现有 RuntimeActor/TaskProgress 上区分知识更新、产物变更和验收推进。对外部反馈保存有界 blocker fingerprint、candidate identity、义务状态和最后一次实质变化；仅 timestamp/batch 变化不清空当前交付停滞。改变分类不能伪造验收，也不能让 advisory 成为新的权限来源。

### 5.3 Context/GC 与 KV 缓存目前不能归因

没有取得 V5 的实际每轮 prompt、fs_read_motive、ContextConsumed、required miss、cold/warm recall、工具表变化及 cache token buckets，因此不能判断重读究竟来自正文回收、摘要装箱、文件变更还是模型行为。

建议冻结一份按模型轮对齐的诊断数据：Task/Directive/模型轮/请求身份、可见正文区间、读取动机、候选哈希、当前 blocker、frontier delta、schema 摘要、实际 cache 命中 token。再比较重复读取的原因与单位验收推进成本。不要在证据缺失时通过放大上下文、取消 GC、固定常驻全部工具或提高总决策额度猜修。

初步动作应是有界的“当前合同＋未解决失败＋下一条复现动作”投影，而不是加入新的通用 DAG Planner 或第二个 orchestrator。

### 5.4 长寿命任务不等于持续模型轮询【设计建议】

V5 提示要求模型保持同一任务、不断阅读反馈，而 run.py 又把控制器生存期绑定到单个模型段。此组合没有清楚区分“任务仍在等待外部验收”与“需要再做一次模型决策”。

建议由既有 RuntimeActor 和 campaign 协调有界的外部条件等待：在明确 blocker 没变、实现版本没变时等待事件或截止时间，而不是持续用模型调用轮询 latest.json。验收窗口内的纯本地负载可以独立运行，但不能未经原合同授权延长预算或截止时间。该方案不要求另建通用 Planner，也不允许把等待状态算作完成。

## 6. 可执行修复顺序（不启动新付费窗口）

| 顺序 | 修改范围 | 必须满足的退出条件 |
|---|---|---|
| 1 | SPEC、prepare、preflight | 在线/静止/WAL 权限一致；资源规模算得过；规范、代码、oracle 绑定同一身份 |
| 2 | oracle 与本地 tests | 本报告 O1–O4 从红转绿；补完整 outbox、重复发布、journal 闭包和并发切点反例 |
| 3 | continuous_load、run、invoke | 真正并发；合法候选可在早期失败后进入独立120分钟窗口；恢复保留请求与generation；故障必须真正触发才计入 |
| 4 | runner/grants/budget/finalize | 授权与时长相容；跨段总额机械受限；实际预留算法与说明相同；所有进程树清理和结果可重演 |
| 5 | Runtime/Context 定向优化 | 变化心跳不解除交付停滞；未决失败持续可见；正文丢失/重读有真实轨迹；保持 Core 和单Actor边界 |

第1–4项先通过无供应商的控制器测试，不把基础可满足性检查也交给付费模型。另开真实窗口前必须保留 V5 原始失败和新的合同差异，而不是改原窗口的预算或验收标准。

## 7. 新增回归规格

- **合同可满足性**：生成最大允许历史规模，计算闭包成员数及字节；超过冻结合同直接 NOT_READY。
- **有WAL的静止/在线源**：明确期望，校验 committed WAL 纳入或合法拒绝；不得一边要求 immutable 一边把 WAL 可见性作为通过条件。
- **早期失败后修复**：第一候选失败，第二候选修复；前者证据保留，后者可获得单独的冻结版本验收窗口。
- **真实并发**：用屏障证明至少两个写者及一次备份区间发生重叠，不以线程对象数量代替覆盖。
- **断点续跑**：恢复旧库，下一条新请求使用正确 generation 与未用 identity；旧身份至多幂等查询，不能假作新增负载。
- **故障门**：未见 exit74 或目标故障状态时，必须 NOT_TRIGGERED；实现正常重试及目录不变性断言。
- **失败持续投影**：WAL 错误之后连续普通 install 成功，失败正文仍可见，直到同义务被证实解决。
- **进展分类**：反复只修改 latest.json 的时间/批次，验收失败保持不变，不得无限延后任务级停滞。
- **授权耗尽**：合法第48/49次、过期、argv不匹配分别有明确原因；续租必须受原总额限制。
- **跨段预算**：剩余额度由账本导出，不信任调用方传入的 rounds；网络重试和主决策分开计算。
- **证据完整性**：原始段丢失或日志坏行时 INCOMPLETE，不从空集合推导成功；汇总只能由对应stage生成。

## 8. 证据索引

全部仓库路径均针对固定 `7224a7baa8910881b9efaa77f14013781137ff49`，除非标明原回执内的测试身份。

| 编号 | 来源 | 用途 |
|---|---|---|
| S01 | docs/experiments/package-endurance-v5/PLAN.md | 计划语义及资源/时长目标 |
| S02 | docs/experiments/package-endurance-v5/RUN_2026-09-21.md | 原执行叙述 |
| S03 | docs/experiments/package-endurance-v5/final-evidence-final-r2/FINAL_STATUS.json | 原分段、费用、规模、失败摘要 |
| S04 | docs/experiments/package-endurance-v5/final-evidence-final-r2/MANIFEST.json | 实际归档范围 |
| S05 | scripts/package_endurance_v5/SPEC.md | 模型规范、immutable、成员上限 |
| S06 | scripts/package_endurance_v5/prepare.py | SPEC复制/保护和规模生成 |
| S07 | scripts/package_endurance_v5/preflight.py | READY实际检查范围 |
| S08 | scripts/package_endurance_v5/run.py | 模型/控制器生命周期、V5账本覆盖 |
| S09 | scripts/package_endurance_v5/continuous_load.py | 串行循环、失败累计、反馈、故障、恢复 |
| S10 | scripts/package_endurance_v5/oracle.py | 判断器源文件；本地副本Gitblob已校验 |
| S11 | scripts/package_endurance_v5/invoke.py | 候选隔离调用与错误封装 |
| S12 | scripts/package_endurance_v5/finalize.py | 汇总硬编码及归档生成 |
| S13 | scripts/runtime_endurance_incremental_runner.py（已读关键区段） | 金额预约、grants、清理、模型段与事件计数 |
| S14 | crates/agent-runtime/src/actor/tools.rs:runtime_execution_attribution | 任务根路径归属 |
| S15 | crates/agent-runtime/src/execution/freshness.rs | 证据及前沿分类 |
| S16 | crates/agent-runtime/src/execution/state.rs（已读区段） | 停滞阈值、持续投影、前沿重置 |
| S17 | crates/agent-runtime/src/execution/classify.rs | fs.read原因诊断已有实现 |
| S18 | .github/workflows/ci.yml；固定提交Actions查询 | 实际CI入口与未运行边界 |
| P01 | probes/probe_v5_oracle.py + probe_results.json | 本地反例及WAL微实验 |

### 公开技术依据（用于核对机制，非替代仓库证据）

```
SQLite URI options: https://www.sqlite.org/uri.html
SQLite Online Backup API: https://www.sqlite.org/backup.html
Python 3.13 sqlite3 transaction control: https://docs.python.org/3.13/library/sqlite3.html
```

## 9. 最终判断

V5 保留了未通过、单TaskId、已结算估算账本等有价值信息，但它还不是一个能清楚隔离“模型能力 / Runtime长流程 / Context策略 / 控制器正确性”的实验。

当前最确定的优先级是修复测试的可满足性、oracle 和运行边界，再针对已有 Runtime 增加“交付进展不被反馈心跳替代”的有界机制与回归。不要直接重写 Context/GC，也不要以追加轮数替代原因定位。
