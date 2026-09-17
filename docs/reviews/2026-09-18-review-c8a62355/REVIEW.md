# c8a62355 全仓范围续审：逻辑所有权、实际交付位置与生命周期边界

审查基线：`c8a623554f762505061cdfdd7466ccaa995a3de0`。提交 UTC 2026-09-17 19:01:13，东京时间 2026-09-18 04:01:13。收尾分支查询仍是该 SHA。

## 0. 范围与证据等级

这是以全仓为范围的持续审查，本轮实际读取 20 个不同文件的全文或指定区间，扩展到未改动的存储／reconcile／Windows 包装器。**不是全仓全部源码、测试、SDK 和脚本的逐行完成回执。**目录盘点、文件读取、调用链分析和执行测试分别记账；详见 [COVERAGE.md](COVERAGE.md)。

源码由 GitHub 连接器按固定 SHA 获取。本环境无 Cargo、Rustc、.NET，Git 访问出现 DNS 失败，未取得可用本地工作树。没有执行仓库 Rust/.NET 回归、真实终端或 Windows 测试、付费模型实验，也未修改／推送仓库。

已执行 [mechanism_probes.py](mechanism_probes.py)：Python 移植的冷目录决策表、工件 capture／broker 控制流，以及 Linux 临时文件上的真实 flock 机制。结果见 [MECHANISM_RESULTS.json](MECHANISM_RESULTS.json)。脚本明确不模拟完整 Rust 类型、WAL 格式、目录 confinement 或供应商；工件页脚文字是简化渲染，不是逐字节重放完整工具。

CI run `35262371575`，attempt 1，最后接口返回 `in_progress`，conclusion 为 null。最近的 jobs 返回六项成功、Windows full Rust test 仍运行；不将父提交的结果移作本 SHA 的验收。见 [CI_OBSERVATION.json](CI_OBSERVATION.json)。

## 1. 结论与优先级

| 编号 | 性质 | 优先级建议 | 直接影响 |
|---|---|---|---|
| G1 | 新静态缺陷；决策表探针通过 | P1 | 冷页仍有 owner，却被 reconcile 当孤儿重新认领；hot/pending 双重所有权、热目录回涨。 |
| G2 | 新跨层反例；控制流探针通过 | P1 | 游标按内部 capture 推进，最终模型只看见 broker 首尾；跟随返回游标漏过中段。 |
| G3 | 新 UTF-8 边界；控制流探针通过 | P2 | 一个字符放不下时跳过当前行，后续短行被接纳、错误编号，并可能报告结束。 |
| G4 | 新库级并发缺陷；真实 Linux 锁机制验证 | P2 | 写者锁随 WAL 代际轮换；过期打开与 compaction 可形成旧写者和先截断后拒锁。正常产品另有 Workspace 外层锁。 |
| G5 | 既有 C0 的已知同类残余，代码复核 | P2 | 通用 Windows host 与 Low-IL wrapper 仍有运行后加入 Job 的窗口，后者忽略分配失败。 |

G1 与 G2 先收口；G3 随工件切片完成；G4、G5 可并行。继续后端长流程和供应商 KV，不把每个观察升级成新的总门禁，不开展全仓重写。

## 2. G1：reconcile 只看热目录，重新认领已经由冷页拥有的 ID

### 源码链

[crates/context-simple/src/engine.rs:3270–3390](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/context-simple/src/engine.rs#L3270-L3390)：`reconcile_store_protecting` 先按预算 hydration，再仅从 `state.external` 建立 map_checksums；resident_ids 包含 heap、Warm、pending_externalize_retry，却不包含 pending_external_cards。`hydration.complete` 作为布尔事实传入 store。

[crates/context-simple/src/store.rs:1640–1855](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/context-simple/src/store.rs#L1640-L1855)：blob 既不在 map_checksums 又不在 resident_ids 时，直接进入 rebuilt_candidates。元数据不完整的判断位于后面的删除分支，**没有阻止重新认领**。

[crates/context-simple/src/store.rs:1930–2025](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/context-simple/src/store.rs#L1930-L2025)：commit_reconcile 只检查 external.get(id)，然后插入从 blob 重建的条目；不移除既有 pending 定位。调用者返回前也没有通过已有 metadata-residency 入口结算。

### 反例与边界

固定热上限 1，热 A，冷 B/C，三份合法 blob 与卡片均存在。hydration 因 HotCap 停止；B/C 未进入已加载 map，也不在 resident_ids。扫描把 B/C 当作孤儿，提交得到 hot A/B/C + pending B/C。

决策表探针得到 hot 1→3、pending 仍为 2、重复 ID 为 b/c。它验证分支组合，**未执行 Rust 引擎**。

本问题不是“文件立即被删掉”。当前可确认的是 owner 唯一性破坏、热预算失守；从原 blob 重建元数据还可能偏离冷卡片捕获的版本，但不声称每次都造成同一后续错误。既有不完整根禁止删除的修复仍有效，不原样重开。

### 最小修复

用完整的逻辑 owner 快照驱动 reconcile：包含 resident/warm/写重试/loaded external/pending cold，并保留版本／claim 身份。未读冷元数据意味着详情未知，不意味着无主。commit 再校验所有位置，只接纳真正孤儿，不用旧 blob 的状态替换已知冷卡片。合法新孤儿接纳也必须走预算结算；必要时背压，而不是任意扩大 hot。

不得通过全历史 hydration、无限 pin 或删除 pending 校验来规避。

### 回归终点

相同 fixture 重复 reconcile，owner 集合与各层互斥不变；冷卡片版本可取回；固定热配置受控；checkpoint/restore 保持身份。加入真正孤儿的正对照、损坏页／瞬时 I/O／预算停留，确认恢复功能未被一刀切关闭。

## 3. G2：capture 游标不等于最终交付游标

### 源码链

[crates/tool-runtime/src/tools/artifact.rs:155–420](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/tool-runtime/src/tools/artifact.rs#L155-L420) 按约 2 MiB 的 capture 上限确定 next_start_line / next_line_byte_offset；[crates/agent-workspace/src/broker.rs:140–280](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-workspace/src/broker.rs#L140-L280) 随后可能将这份输出保存为新工件，并只保留首尾 16,000 字符。

[crates/agent-workspace/src/broker.rs:1–139](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-workspace/src/broker.rs#L1-L139) 的截断更新只面向声明 start_line/end_line/covers_file 的 file-window 元数据；artifact.read 当前返回的是其自己的 paging 字段。无论是否新增一个截断标志，**原游标仍没有按最终展示位置重算**。

上一轮 F1 的动态 end_line 默认和 F2 的行内偏移确实已增加，本项是它们与 broker 的组合反例，不是声称旧修改不存在。

### 普通日志反例

500 行，每行 150 个内容字符加换行，总计 75,500 字节。工具按 200 行一页读取，每页内部 capture 未截断，但 broker 把约三万字符裁成首尾。沿原始工件的继续参数执行 1→201→401，位于第 100、300 行的 marker 始终没进入最终正文，最后 has_more=false。

### 超长行反例

3 MiB 单行。内部第一页推进至原始字节 2,093,952；第二页结束。最终两页均为 16,000 字符。1 MiB 与 2.5 MiB 的 marker 不可见，恰在 2 MiB 附近的旧测试 marker 却可见，因为它位于第二页前部。这解释了“某个 sentinel 看到”为什么不能证明完整取回。

原文件／工件仍保留；手动选其他偏移可能读到遗漏内容。缺陷是**按产品返回 continuation 行走无法完整交付，且结束说明过强**，不是所有读取接口永久不可达。

### 最小修复

让工具分页直接受最终 model-content 预算约束，包含行号、footer、引用与包络成本；或让可信裁剪器携带源区间映射并同步修改继续位置。扫描位置、capture 位置、最终交付位置应独立表达。首尾不连续预览不能作为连续区间已交付的证明。

测试应消费真实 tool→broker→runtime 的最终正文；用整组唯一块 ID 或源区间集合核对无缺口／无重叠，而不是只放一个恰好位于页头的 marker。改变工具 budget、行长和 UTF-8 后仍成立。每次继续都有实际推进；报告结束时已展示／明确跳过的范围得到证明。

## 4. G3：多字节字符无法装入余量时，不能继续接纳后面的短行

同一 [crates/tool-runtime/src/tools/artifact.rs:155–420](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/tool-runtime/src/tools/artifact.rs#L155-L420) 在 `chunk.is_empty()` 时直接 `continue`，没有固定 first_unshown 或建立 mid_line_cursor。后续短行仍可能被捕获，并让 last_captured_line 越过空洞。最后按枚举序号渲染，把真实第 3 行标成第 2 行。

有效 UTF-8 反例：第一行（含换行）消耗 CAPTURE_CAP−1 字节，剩 1；第二行 `界\n` 的首字符需要 3 字节，take 退到 0；第三行 `x\n` 的 x 放得下。探针得到真实捕获行 [1,3]，显示行 [1,2]，window_truncated=false，has_more=false，未给出 coverage footer。

这不依赖非法 UTF-8，也不依赖 broker 二次裁剪。修复应在第一个不可展示位置停止后续 capture，继续位置仍指向该行／偏移；行号保持源位置，不能把后行压紧。分隔符预算也需统一。随 G2 一起实施，但单独保留这一反例。

## 5. G4：WAL 代际锁不足以证明 journal 生命周期的唯一写者

### 源码与平台语义

[crates/agent-storage/src/lib.rs:1–190](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-storage/src/lib.rs#L1-L190)：open 先读取 metadata 选择 generation，再打开对应 WAL 并 try_lock；锁成功后使用先前缓存 metadata，没有重读代际。

[crates/agent-storage/src/lib.rs:635–810](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-storage/src/lib.rs#L635-L810)：compact 打开下一代固定路径时使用 create+truncate，写入全部记录，然后才 try_lock。发布 metadata 后替换 writer.file、释放旧文件锁并删除旧代 WAL。

Rust File 文档说明 Unix 当前使用 flock，读写与锁交互依平台而异，不能假定未持锁写操作自动被阻止：[官方 File 文档](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock)。

### 条件竞态

B 读取 G1 metadata、打开 G1，暂停在取锁前。A 完成 G2 发布，释放 G1 锁并 unlink G1。B 通过自己仍打开的 G1 句柄取得锁，按旧 metadata 恢复为健康写者。A、B 锁的是不同代际文件。

B 再 compact 到自己推导的 G2 路径，先 truncate/write，后才发现 G2 已被 A 锁定。拒锁发生得太晚。

实际 Linux 临时文件实验验证：旧代句柄可在 A 释放后加锁，且 w+b 可在另一个 flock 仍存在时把当前文件从 21 字节截成 0；随后请求锁才被拒。**未执行 FileOperationJournal 的真实编码／恢复／compaction，实验不代表完整 Rust 端到端复现。**

### 影响范围必须限定

[crates/agent-workspace/src/journal.rs:1–240](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-workspace/src/journal.rs#L1-L240) 表明正常独立 Workspace::open 还会持有稳定的 effect-journal 外层锁。它会阻止普通的第二个工作区宿主，因此本报告**不声称已经绕过该外层锁，或普通 TUI 双开必然损坏 WAL**。问题位于公开可复用的 FileOperationJournal 自身，以及不受同一外层锁保护的使用方式。

### 最小修复

写者锁应有不随 WAL 轮换的稳定身份，在读 metadata 之前取得并持有整个 journal 生命周期。候选代际先用安全创建／验证流程建立，再写入；不得先截断可能属于活动写者的路径。保留格式、祖先标记与恢复兼容。若使用稳定 lock 文件，它不应在持有者生命周期中被 unlink 后重新创建。

回归用 barrier 精确控制 open→读取 metadata→取锁与 compact 发布的交错；竞争方拒绝时活动 WAL 字节必须不变。保留普通 Workspace 独占锁正对照。Windows 的锁和删除语义需独立执行验证，不将 Linux 实验推广为 Windows 结果。

## 6. G5：C0 已修复一条路径，通用 Windows 入口仍有已知窗口

C0 回执已经记录：验证 runner 改为挂起创建→加入 Job→恢复，通用 ProcessHost 与 integrity wrapper 尚未覆盖。此处作为**已知残余的代码核对**，不是把旧 CI 失败归到新 SHA。

[crates/agent-process/src/host.rs:460–750](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-process/src/host.rs#L460-L750)：通用 host 仍是普通 spawn 后分配 Job。[crates/agent-process/src/integrity.rs:135–200](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-process/src/integrity.rs#L135-L200)：Low-IL wrapper 同样普通 spawn，且使用 `let _ = assign_pid_to_job(...)` 忽略布尔结果。

Microsoft 说明 Job 关联后的子进程通常继承关联；这不能追溯覆盖关联之前已创建的所有后代：[AssignProcessToJobObject](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject)。外层 Job 有时已经保护 wrapper 及后代，因此不宣称每次内部关联失败都会逃出全部保护；应依据实际已建立的边界判断。

沿现有 suspended-create 修复提取小型公共“已建立 containment 的子进程”入口：在目标代码运行前完成 Job 关联；必要关联失败就回收尚未启动的 child；恢复成功后才交出运行句柄。attestation 必须来自成功证据，不来自创建了 Job 对象或忽略的结果。工具权限与 broker 意图规则仍归现有 Core，不合成巨型通用授权 runner。

每个生产 spawn 入口都要用快速派生后代、关联失败和宿主死亡的确定性屏障回归，不只是 proof 路径的 helper 测试。本轮没有运行 Windows 回归。

## 7. 本轮应保留的进展与未升级的怀疑

- Headless 已把 TurnFailed 作为终态，而不是一见 Failure/ModelUsed 就退出；共享状态也单独处理用量与运行生命周期。来源：[crates/agent-tui/src/cli.rs:310–650](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-tui/src/cli.rs#L310-L650)；[crates/agent-runtime/src/status.rs:1–200](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-runtime/src/status.rs#L1-L200)。
- 明确加载的工具由 directive cohort 保留，仍只是需求／驻留根，不是权限授予。来源：[crates/agent-runtime/src/actor/tools.rs:1810–1920](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-runtime/src/actor/tools.rs#L1810-L1920)。
- 失败回合不直接安装“已提交 resume”是代码明确的事务边界。本轮没有足够新证据把它定性为状态丢失；不能为了保留投影而让未提交结论成为权威。
- 新 KV sequence 已经走真实 Compose/Actor/工具/Provider，并断言真实写入和恢复后读取。来源：[crates/agent-compose/tests/kv_production_sequence.rs:600–860](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-compose/tests/kv_production_sequence.rs#L600-L860)。不再按旧的 DTO-only 或只读审批却声称写成功的问题派工。
- 本轮对 SDK 的读取止于 Envelope/ProtocolDto 的指定区间，未宣称已经覆盖所有新终态事件消费者。

## 8. 供应商 KV 与维护性下一步

在已有 production sequence 增加三类断言即可，不建设新框架。

1. 稳定边界验证整个 input[0..=B0/B1] 及参与匹配的 tools/schema，而不只验证断点所在最后一项。当前末段 B0 断言主要比较 input[b0]，某些相邻请求还有 first_input_diff 对照；不要把它写成已验证每一轮完整前缀稳定。来源：[crates/agent-compose/tests/kv_production_sequence.rs:860–1063](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-compose/tests/kv_production_sequence.rs#L860-L1063)。
2. 将 G2/G3 的工件实际交付区间放进轨迹，验证全部必需正文，而不是错误跳读带来的低 Token。
3. 费用对照记录 input/output/cache-read/cache-write/cache-miss、attempt、主调用与维护调用、缺测状态。当前 LedgerRow 只保存 input/output/UsageIdentity，且本地服务器的 usage 是脚本值；不能据此报告真实命中率或价格收益。来源：[crates/agent-compose/tests/kv_production_sequence.rs:330–420](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/c8a623554f762505061cdfdd7466ccaa995a3de0/crates/agent-compose/tests/kv_production_sequence.rs#L330-L420)。

官方说明精确前缀匹配与写入／读取需要分别观察；路由 key 不是相同内容的替代：[Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)。真实端点接受、实际命中与同质量净费用均未在本轮执行，保持 NOT_RUN。

维护性验收聚焦四条不变量：逻辑 owner 不随驻留变化；continuation 不越过尚未交付的源位置；journal 写者身份不随 WAL 代际变化；进程不能先运行后补必需隔离。这四条各自只有一个生产维护入口。改行为与大段代码搬移分开提交。

## 9. 实施与交付索引

具体所有权、回归、停止条件和命令在 [NEXT_ACTIONS.md](NEXT_ACTIONS.md)。源码读取范围在 [COVERAGE.md](COVERAGE.md) 和 [SOURCE_MAP.json](SOURCE_MAP.json)。

不要把本报告全文追加到 CURRENT/NEXT；只替换当前动作并链接原回执。GUI 仍后置。目标仍是一个能真实取回证据、可靠恢复、及时中断且成本可解释的长流程主体，而不是审查项永远归零。
