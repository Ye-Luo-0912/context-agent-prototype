# A0 — proof_supervision CI「exact proof tree exit」失败根因调查

日期：2026-09-13。审查基线：`489c89cd`（失败 run `34754942152`）；本地工作树 HEAD `aaeb827f` 加并行线在飞编辑。
范围：根因调查＋修复＋确定性回归；未 commit / 未 push。

## 1. 根因结论

**分类：生产监督健壮性缺陷（环境触发），叠加夹具缺陷；不是纯环境时延，不是测试身份比较错误。**

一句话：exact proof tree 的宿主死亡遏制（Linux＝UnixStream EOF 看门狗进程、Windows＝KILL_ON_JOB_CLOSE Job 对象）是在**spawn 出 proof worker 之后**才建立的，建立失败时**静默降级**（eprintln 进测试重定向的 stderr 文件，无人读取），建立被延迟时**无人等待它**。失败 run 里遏制根本不存在：宿主被 SIGKILL 后 leader/member 带着完整身份继续睡眠（各自 30 秒自过期），测试只等 20 秒，盲超时。

这不是「观察被负载拖慢」：两种遏制一旦建立都是内核侧毫秒级动作（EOF→`kill(0, SIGKILL)`；Job 句柄关闭→内核杀树），10ms 轮询循环不可能晚 10–20 秒才观察到。`d171a142` 把早先 Windows 失败（run `34748602228`）记载为「负载抖动、观察变慢」是**错误的机制解读**——那次同样是遏制缺失，靠 worker 30 秒自过期在 20 秒窗口内「假绿」。本轮按审计要求未按抖动忽略。

## 2. 证据链

1. **失败 run `34754942152`（Linux part 2）完整日志**（`gh run view --log-failed`）：
   - `proof_supervision.rs` 独占测试二进制、`running 1 test`（11:43:39.345 起），失败于 11:43:59.949，**全程 20.60s** → kill 发生在 ~0.5s，树等待耗尽整整 20s。
   - 超时点身份报告：`leader pid 4433 inspect Ok(Running(... token "...:45442")); member pid 4434 inspect Ok(Running(... token "...:45442"))` —— 两个进程在 kill+20s 仍真实存活（非 PID 复用、非观察错误）。
   - 收尾的 runner 孤儿清理在 11:44:00.167 `Terminate orphan process: pid (4433) (crash_child)` —— 佐证 leader 确实活到了测试退出之后。
2. **PID 相邻性**：leader=4433、member=4434，且两者 `/proc/stat` starttime 同 tick（45442）→ member 是 leader fork 自己之后 <10ms 内 fork 的。宿主侧看门狗 fork 按程序顺序发生在 leader fork 与 member fork 之间；若它曾成功 fork，member 将 ≥4435。**member=4434 证明 member fork 时刻不存在看门狗**；而看门狗一旦 fork 就活到 EOF 并杀组（leader/member/自己全灭）——「kill+20s 仍 Running」进一步证明**看门狗直到宿主死亡后都不存在**。
3. **代码路径核对**（`tool-runtime/src/tools/process.rs::execute_invocation`）：spawn leader → `supervision::lease`（含 **两次 fsync**：文件 `sync_all` ＋目录 `sync_all`）→ `ProcessTreeGuard` → `HostDeathWatchdog::arm`。arm 失败路径只有一行 eprintln（进宿主被重定向的 stderr 文件）然后 `None` 降级，注释明说 "degrades to no containment"。因此两种机制都指向同一缺陷形态：(a) 看门狗 spawn 的 fork 遭遇瞬时失败（EAGAIN/ENOMEM）→ Err → 静默降级；(b) 宿主在 fsync/锁窗内被 kill（本例 kill 在 spawn 后 ~0.4s）→ arm 尚未 fork。日志无法进一步区分两者（stderr 文件在 tempdir 内随测试销毁），但**两者共享同一产品缺陷**：遏制「是否已建立」既不被验证、失败也不可见。Windows 侧同形：`HostDeathJob::create()` 失败是静默 `None`，assign 被拒只有 eprintln。
4. **Windows「抖动」复核**（run `34748602228` 日志）：旧版 10s 等待测试在 09:02:13.45 起跑、09:02:24.45 失败，全程 ~11s，签名与 Linux 完全一致（树存活、盲超时）。同上，无「观察延迟」机制 —— 该 run 应重读为遏制缺失（assign 被拒/降级），而非负载抖动。
5. **本机（Windows）可复现降级形态**：临时注入强制走「assign 被拒」降级分支（scratch 编辑，已完全还原，`git diff crates/tool-runtime` 为空）后，宿主死亡树不被清理 —— 与 CI 签名一致。

**排除项**：`owned_process_exited` 的身份比较逻辑正确（超时报告里的 Running 是真实存活）；`crash_child` 的 `--proof-worker`/`--proof-member` 进程组归属正确（leader `process_group(0)`，member 继承）；生产监督账本（`reconcile_children`）按设计只作用于「下一次启动」，与本测试的 20s 窗口无关。109bbf93（绿）→ 489c89cd（失败）之间 tool-runtime/agent-process/crash_child **零改动**，排除「新 SHA 引入回归」——是既有缺陷按概率显形。

## 3. 改动

| 文件 | 内容 |
|---|---|
| `crates/agent-process/src/watchdog.rs` | ①看门狗 spawn 增加**瞬时失败有界重试**（`spawn_watchdog_with_transient_retry`：仅 EAGAIN/ENOMEM，3 次、100ms 间隔；永久错误立即返回），压缩「单次 clone 失败 → 静默失去遏制」窗口；stdin 读半每 attempt `try_clone`，成功后 drop 原件，保持「管道恰好两个持有者」契约。②新增两个 unix 单测（重试至成功；永久失败不重试不退避）。 |
| `crates/agent-process/src/lifecycle.rs` + `lib.rs` | 新增 Linux 只读观察助手 `process_group_members(pgid)`（/proc 扫描组内**非 zombie** 成员；只读、可跳过不可读项，绝不制造 pid）。遏制诊断按「组成员身份」而非 pid 号识别无名看门狗。 |
| `crates/agent-process/tests/process_groups.rs`（新） | 组观察集成测试（unix/Linux 门控）：加入的成员可见、zombie 不算活成员、新组只含 leader。 |
| `crates/agent-compose/tests/proof_supervision.rs` | **确定性屏障取代盲等**：①Linux 在 kill 前先等「遏制已建立」事件（leader 组内出现除两 worker 外的活成员，5s 上限；等不到＝立即失败并附宿主 stderr）；②kill 后的树等待循环里，宿主 stderr 出现降级标记（`host-death watchdog arm failed` / `host-death job assign skipped`）即**立即失败并引用原文**；③超时消息保留精确进程身份并**追加宿主 stderr**；④改写注释，纠正 34748602228 的「抖动」解读。等待时长维持 20s，未加大。 |

边界遵守：未触碰 `crates/agent-compose/src/compactor.rs`（并行线在飞）、`crates/tool-runtime/**`（仅在红检查演示中临时注入一行并已还原，`git diff` 为空）、未 commit/push。

**建议的后续片（B 线所有权，本次越界未做）**：`tool-runtime::execute_invocation` 里把 `arm` 提到 fsync 重的 `supervision::lease` 之前，并把 host-trusted proof 通道的 arm 失败从「降级」改为「fail closed」（杀树＋typed error）——这是把本缺陷彻底关死的产品侧收口。

## 4. 红/绿证据

- **夹具红检查（本机 Windows 实际执行，注入已还原）**：强制降级（assign 被拒分支）下：
  - 旧测试（HEAD 版）：`FAILED ... finished in 21.05s` —— 盲超时、无归因，正是 CI 签名的复现。
  - 新测试（同注入）：`panicked at ...:166: host-death containment silently degraded (host-death job assign skipped): process 6356 is already confined by an outer job`，**1.01s** 即失败且带 OS 级归因。
  - 撤掉注入后新测试全绿：`test result: ok. 1 passed ... finished in 1.07s`。
  - 该红检查同时覆盖 Linux 变体的失败形态（标记同为两平台降级路径）；Linux 前置屏障的端到端行为待 CI run 记录（见 §6）。
- **看门狗重试红检查（逻辑）**：`transient_spawn_failures_are_retried_until_the_watchdog_arms` 在无重试的单次尝试实现下必然以 `Err(EAGAIN)` 失败（调用方随之静默降级）——正是 34754942152 的机制。该单测为 `#[cfg(unix)]`，Windows 本机不可执行（见 §6）。

## 5. 验证输出（本机 Windows，实际命令与结果）

- `cargo test -p agent-compose --test proof_supervision` → `ok. 1 passed; 0 failed`（1.07–1.5s，多次运行一致）。
- `cargo test -p agent-process` → 11 个目标全 `test result: ok`（lib 38、host 24、sandbox 6、rlimits 5 等，0 失败）。
- `cargo clippy -p agent-compose -p agent-process --all-targets` → 0 警告。
- `cargo fmt -p agent-process -- --check` 通过；改动文件 rustfmt 通过（`agent-compose/src/compactor.rs` 存在 fmt diff 属并行线在飞文件，非本片）。
- `cargo check -p agent-process --all-targets --target x86_64-unknown-linux-gnu` → 通过（unix 门控新代码与测试可编译）。

## 6. 未验收项（如实）

- **Linux 端到端行为待 CI run 记录**：前置屏障（wait_for_armed_containment）、组观察集成测试、看门狗重试单测均为 unix/Linux 门控，Windows 本机只做了目标编译检查（§5），未实际执行；修复对 CI run 34754942152 所代表的真实 Linux 失败是否闭合，以 CI Linux part 2/part 1 后续 run 为准。
- 根因的两种具体触发（fork 瞬时失败 vs. 宿主在 fsync 窗内被 kill）凭现有日志**不可最终二选一**；回执如实按共同缺陷形态（遏制未建立＋静默降级＋无人等待）定性并各给修复，不宣称单一触发已确证。
- §3 的 tool-runtime 侧收口（arm 前置＋proof 通道 fail closed）未做（越界），是本缺陷的彻底关闭项。
