# context-agent-prototype 深入续审报告

**日期：2026-09-06**  
**固定源码：`12c86283b8d5991e9f17a07f14871dcf39d65066`**  
**对象：`Ye-Luo-0912/context-agent-prototype`**  
**状态：部分源码深入审查 + 有针对性的实际执行；不是全仓审查完成证明。**

## 0. 结论与边界

这轮尝试了完整克隆，但运行容器无法解析 `github.com`，克隆失败；源码归档也没有成功取得。容器没有 `cargo`/`rustc`。因此，本报告不声称已经完整克隆，不声称全量阅读了仓库，不声称本地 Rust 构建、Clippy、测试或 Windows 故障注入通过。实际克隆错误在 `logs/clone_attempt.json`，环境检查在 `environment.json`。

随后通过 GitHub 连接器固定 SHA 读取文件，深入补查上轮覆盖较薄的持久化、文件句柄、进程监督、Provider、MCP、打包与文档。开始和结束两次检查的 main 都是上述 SHA，与上一轮交付报告的源码相同。下述问题是对同一源码的新发现，不是声称仓库新引入了回归。

本轮登记了 **29 个不同正文路径**：18 个取得全文，11 个取得部分范围；源码正文涉及 8 个 crate。完整读取的是这些文件，不是这些 crate 的所有文件。两份现有阅读登记合并后有 46 个不同路径，这也不是完整仓库文件总数。根递归树的工具返回发生截断，所以不能给出可信的全仓覆盖百分比。详见 `READ_COVERAGE.csv`、`CUMULATIVE_READ_COVERAGE.csv` 和 `AUDIT_STATUS.json`。

本轮新增 **13 个问题项：6 项 P1、6 项 P2、1 项 P3**。这些是修复优先级，不是 CVSS 评分；P1 表示在受影响的恢复、发布或可用性路径上应优先处理，并不表示所有默认用户运行都必然触发。

核心结论：**目前最有价值的工作不是重写 Runtime/Context/GC，而是把异常路径中的“事实”与“承诺”对齐：已发布不等于未发生，返回错误不等于没有写盘，发出 kill 不等于已退出，宿主信任不等于不需生命周期管理，生成 checksum 不等于打包了本次构建。** 这些都是现有模块的闭环问题，不需要新增通用 Planner、数据库或另一层权威。

所有建议均为审查建议；没有修改远端仓库，没有创建 PR，没有执行真实模型任务，没有把新问题伪装成已修复。

## 1. 实际执行与未执行事项

| 项目 | 本轮事实 | 不能据此声称 |
|---|---|---|
| Git clone | 实际执行，DNS 失败，退出码 128 | 已取得完整 checkout |
| GitHub 固定版本读取 | 固定 SHA 的文件正文、部分目录树、CI job 状态 | 所有文件、历史 evidence、依赖和二进制都已审查 |
| 远端 CI | run `33986702977` 的 6 个 job 均返回 success | 本地重跑通过；本报告新增反例已经在 CI 测过 |
| FIFO 探针 | 同类 Unix 打开标志会阻塞；NONBLOCK 返回后可识别 FIFO | 仓库 Rust 集成测试已复现 |
| 宿主硬退出探针 | 独立进程组内的静默子进程可在父进程 SIGKILL 后存活；已全部回收 | 已运行 Agent/验证配方的真实 crash-resume 测试 |
| 打包脚本探针 | 仓库原版 dist.sh + 桩 cargo，两个错误打包场景与一个失败退出对照实际执行 | 已编译 Rust、已执行 Windows PowerShell |
| 本地全工作区检查 | 未执行：缺少源码 checkout 和 Rust 工具链 | fmt/check/clippy/test 或 release 业务测试通过 |

远端 CI 状态摘要在 `REMOTE_CI_OBSERVED.json`。它是 API 结果的整理，不是原始测试日志。常规 CI 执行 debug 配置的业务测试；另有 `package.yml` 做 release 构建、缺 key 拒绝和 demo doctor smoke。因此，准确的缺口是 **release 业务行为与部分异常路径未被这些 smoke 覆盖**，不是“项目没有 release 构建”。本轮没有核实当前 SHA 的 package workflow 实际运行结论。[S-CI](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/.github/workflows/ci.yml)[S-PackageWorkflow](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/.github/workflows/package.yml)

### 本地探针的可重复性

`source-excerpts/scripts/dist.sh` 是通过连接器返回内容还原的 1,210 字节文件。按 Git blob 格式计算 SHA-1 后，与仓库 blob `b3a3f9a6326df15054cd55e430387c3aeec75008` 相同。它是本报告中唯一保存的完整仓库源码文件，不是一个部分 checkout 的伪装。探针程序在 `probes/`，对应 JSON 是本轮实际输出。

FIFO 与宿主硬退出探针仅适合受控的 Linux 临时环境。探针有超时、终止和回收步骤；JSON 记录的退出与回收状态均已检查。毫秒计时只是证明“是否阻塞”，不作为性能基准。

## 2. STORAGE-01：Windows 元数据删除窗口与错误的新建判断【P1】

**位置：** `agent-storage/src/lib.rs` 的 `replace_file`、`FileOperationJournal::open`、`load_or_create_authority_metadata`、`compact_locked`。[S-Storage](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-storage/src/lib.rs)

### 代码事实

Windows 的替换实现先 `remove_file(to)`，再 `rename(from, to)`，两步之间存在目标 metadata 不存在的窗口。启动时，metadata 不存在就默认 generation 1；只有“metadata 已存在但它指向的 WAL 缺失”会明确拒绝。随后代码可创建 generation-1 WAL 和新的 journal id。

压缩成功后会尝试移除旧 WAL。因此，在已经完成过一次压缩、原始 generation-1 WAL 已不存在的情况下，下一次压缩若在删除旧 metadata 后、重命名新 metadata 前崩溃，磁盘可能保留 `.g2`、`.g3`，却不再有 metadata 和基础 WAL。

### 可推导的失败链

```text
已经完成 g1 → g2，基础 WAL 已删除
  → 开始 g2 → g3
  → 新 WAL 写入
  → Windows 删除旧 metadata
  → 此处崩溃，rename 尚未发生
  → 重启发现 metadata 不存在
  → 默认 g1，创建空基础 WAL 与新 journal id
  → 既有 .gN 没有参与新建判断
```

这违反了“已存在权威状态不能被误判为空初始化”的要求。问题不只是 Windows 目录同步能力的限制，还包含显式 unlink 与 rename 之间的逻辑窗口。

### 证据限制与保护因素

这是源代码条件路径确认，未执行 Windows/Rust 崩溃测试；没有观察到实际重复副作用。Checkpoint 的 journal identity 校验、其他恢复日志可能在部分产品路径进一步拒绝，但不能替代存储打开阶段对既有代际的识别。

### 最小修复

避免先删目标的替换流程；缺 metadata 时区分全新目录与存在 WAL/代际残留的目录。后者默认进入 RecoveryRequired，或经显式、完整校验的恢复流程处理。**不能直接选择文件名中最大的 `.gN`**：它可能是未发布或部分写入的新代。

针对已有成功压缩后的第二次压缩补实际进程 kill-point 测试；现有“制造 metadata 写失败，然后手工恢复备份”的测试不能证明上述崩溃窗口安全。

## 3. STORAGE-02：压缩发布后失败，旧 writer 仍被认为健康【P1】

**位置：** `persist_authority_metadata`、`compact_locked`；Core 的 `compact_authority_journal` 与 `append_transition`。[S-Storage](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-storage/src/lib.rs)[S-CoreOperation](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-core/src/operation.rs)[S-CoreKernel](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-core/src/kernel/mod.rs)

### 代码事实

压缩顺序中存在：新 WAL 写入/同步 → 新 metadata rename 发布 → 父目录同步 → 新文件 seek → 替换内存 writer → 删除旧 WAL。

若 metadata rename 已成功，但后面的目录同步失败；或 metadata 已完整发布，新文件 seek 再失败，函数可能在内存 writer 切换前返回错误。此时没有设置 `writer.failed`，内存仍持有旧代文件和旧 metadata。

### 风险

磁盘公开入口已经指向新代，当前进程却继续认为旧代可写。调用者在显式压缩失败后继续追加时，可能把新记录写到重启不会再选择的旧代。第二次尝试压缩也不能不经对账就重用同一代际路径。

**重要的上层保护：** 普通 `append_and_sync` 出错会由 Core 的 `append_transition` 设置恢复围栏，因此自动压缩沿普通追加报错的路径已有防护。显式 `compact_authority_journal` 只是转发错误，没有相同的围栏处理。本项不是“所有存储错误都会继续执行”。

### 最小修复

明确发布前与发布后的失败语义。发布后不确定性必须形成 sticky failure / recovery fence，拒绝旧代追加，直到重新核实当前公开代际。把不必要的可能失败操作放在发布前；不能把一个普通 `Err` 当成“事务没有发生”。

补两个定点故障：metadata rename 后目录 sync 失败；metadata 发布后 seek 失败。分别测试直接 journal API、显式 Core compact API 和普通 append 自动压缩路径；最后必须验证重开后的可见历史与错误后追加的拒绝行为。**本轮未执行这些 Rust 故障注入。**

## 4. PROCESS-01：宿主验证的硬崩溃监督责任缺失【P1】

**位置：** `tool-runtime/src/proof_runner.rs::verify_exact` 和 `tools/process.rs::execute_invocation`。[S-ProofRunner](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/proof_runner.rs)[S-ToolProcess](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/tools/process.rs)

### 代码事实

宿主配方执行传入 `host_trusted: true` 与 `effect_context: None`。普通工具进程会登记 `persist_spawned_process`；宿主可信分支将 pid 登记值直接置为 None。代码用父进程中的 `ProcessTreeGuard`、`kill_on_drop` 和超时负责清理，并据此注释“没有东西会活过该次运行，崩溃恢复无需回收宿主证明子进程”。

### 根本区别

普通 Future 取消/Drop 与宿主进程被 SIGKILL 不是同一回事。父进程中的 Drop guard 在硬终止时不会运行；父进程中的异步超时也消失。仅让子进程成为独立进程组，不会把其生存期绑定到父进程。

静默子进程即使失去 stdout/stderr 的读端，也不一定马上退出。在有宿主验证配方的实际路径中，它可以继续工作甚至写入 workspace；恢复日志又没有对应的宿主进程监督记录。

### 本轮实际执行

Linux 探针创建同类“独立进程组、没有父进程死亡约束”的静默子进程，然后 SIGKILL 宿主。子进程继续存活，随后由探针显式杀死整个子进程组并回收。父、子两者的回收结果均为 true，记录在 `probes/host_crash_probe.json`。

**它只证明操作系统触发机制，并未执行仓库 Rust 代码、真实验证配方或产品恢复。** 普通 Future 被 drop 时，当前 ProcessTreeGuard 仍是有效的现有保护，不能忽略。

### 最小修复

把“宿主授权、不走模型审批”和“仍必须管理进程生存期”分开。可以复用监督原语记录宿主进程真实身份并在启动时对账，或提供经过验证的 OS 生命周期包含机制。不能伪造 Core operation/effect identity 来填洞，也不能仅依靠可复用的裸 PID。

父进程死亡信号通常只涉及直接子进程，不能未经测试就宣称覆盖所有后代。Windows Job 与 Unix 进程组/身份恢复的保证应分别说明。补真实配方运行中杀宿主、再次启动、确认无旧进程继续变更 workspace 的回归。

## 5. WORKSPACE-01：普通 confined open 会阻塞在 FIFO【P1】

**位置：** `agent-workspace/src/confined.rs` 普通 `open_existing` / `open_staged_for_cleanup`；`runtime_facts.rs::project_markers`。[S-Confined](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-workspace/src/confined.rs)[S-RuntimeFacts](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-workspace/src/runtime_facts.rs)

普通 Unix 打开使用 `O_RDONLY | O_NOFOLLOW | O_CLOEXEC`，没有 `O_NONBLOCK`。`O_NOFOLLOW` 防链接跟随，并不把任意目录项变成普通文件。FIFO 的只读阻塞打开在没有写端时会等待；检查打开后文件类型不能解决“已经阻塞在 open 内”的问题。[E-POSIX](https://pubs.opengroup.org/onlinepubs/9799919799/functions/open.html)

`project_markers` 会对诸如 `Cargo.toml` 的标记执行同步 `open_existing(...).is_ok()`。因此，工作区中名为 Cargo.toml 的 FIFO 可让项目标记收集卡在打开操作。这里有真实调用方，并非只对一个从未使用的私有函数提出假设。

同一文件中的 recovery 专用打开路径已经采用 NONBLOCK 与普通文件检查，说明可原地复用正确模式。但项目标记本身可能包括目录，例如 `.git`，不能简单把所有标记都改成“必须普通文件”。

本轮 FIFO 探针实测：原有同类 flags 在 watchdog 时间内不返回；增加 NONBLOCK 后立即取得句柄，fstat 显示 FIFO、非普通文件。临时子进程已终止并回收。详见 `probes/fifo_open_probe.json`。这不是 Rust 集成复现，也不是整个 Actor 可取消性已经被完整证明。

建议：标记发现使用不跟随链接的元数据检查；正文读取使用非阻塞打开与同句柄类型检查，再进入受字节约束的读取。补 FIFO、普通文件、目录、链接的分类测试，且用独立测试进程避免坏实现卡住整个测试框架。

## 6. PACKAGE-01：打包输入不一定来自本次构建【P1】

**位置：** `scripts/dist.sh`、`scripts/dist.ps1`、`.github/workflows/package.yml`。[S-DistSh](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/scripts/dist.sh)[S-DistPs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/scripts/dist.ps1)[S-PackageWorkflow](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/.github/workflows/package.yml)

两份脚本接受目标目录参数，但 cargo build 未使用这个参数；复制操作却从参数目录读取。它们又复用 `dist/<version>`，没有建立干净 staging，checksum 会把目录中的旧文件一起签入。是否构建/复制 context service 由此前二进制是否存在决定，而不是由显式打包配置决定。

### 真实脚本测试

| 场景 | 本轮执行结果 |
|---|---|
| 默认 target 生成 FRESH，自定义 target 已有 STALE，脚本从自定义目录复制 | 脚本退出 0，包中是 STALE 主程序 |
| dist 同版本目录预先留有旧 helper，本次不构建 helper | 脚本退出 0，旧 helper 留在包中且被 SHA256SUMS 收录 |
| 桩 cargo 明确退出 17 | Bash 脚本退出 17，没有完成打包；这是有效的失败退出对照 |

详见 `probes/dist_script_probe.json`。执行对象是 Git blob 校验一致的仓库原脚本，构建器是临时桩，没有真实 cargo/Rust。Windows 脚本未执行。

PowerShell 还有另一点：脚本只设置 `$ErrorActionPreference = "Stop"`，未显式检查 `$LASTEXITCODE`。在没有启用相应 native-command error preference 的环境中，外部命令非零退出并不自动等同于 PowerShell 终止异常，所以有旧产物时仍可能继续复制。应显式检查原生命令退出状态，不能用 Bash 的正确行为替 Windows 背书。[E-PowerShell](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_preference_variables?view=powershell-7.5)

当前干净 CI 默认目录可规避部分场景；本轮并没有证明已经发布的 CI 包错误。风险在脚本所支持的自定义目录、增量重打包及失败路径。

最小修复：构建、定位和复制使用同一显式 target dir；独立、干净的 staging；helper 选择显式化；native exit code 检查；按实际文件生成校验和，并记录 source SHA、Cargo.lock 摘要、target/profile 与构建配置。不要通过粗暴删除用户自定义构建目录解决 staging 问题。

## 7. PROVIDER-01：错误响应先全文读取，后截断【P1】

**位置：** `provider-openai/src/lib.rs` 的 Chat 与 Responses 非成功 HTTP 分支。[S-Provider](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/lib.rs)

两条分支都调用 `response.text().await`，得到完整 body 后再截成最多 512 字符用于错误文字。16 MiB 的 stream cap 只覆盖成功的 SSE 路径，无法限制此前的错误 body 全量分配。reqwest 的 text 接口是完整文本读取，而不是带上限的前缀读取。[E-Reqwest](https://docs.rs/reqwest/latest/reqwest/struct.Response.html)

一个很大的代理错误页或持续返回内容的非 2xx body，就可以让错误处理比正常输出拥有更大的读入成本。默认 HTTP client timeout 能限制部分等待；重试包装也可能提供额外取消保护，但时间上限不等于字节上限，不能把异常分支的内存边界交给它们猜测。

建议共享 bounded-error-body reader：在每块 bytes 接收时计费，超过上限后停止；在解码前限制字节；保留 status、Retry-After 和显式截断标记，并覆盖取消/deadline。返回错误时不应把“截断的错误消息”写成“完整 body 已读取但显示较短”。

本项为静态确认，未执行 HTTP/Rust 压力或 OOM 测试。

## 8. WORKSPACE-02：Windows 拒绝路径泄漏原始句柄【P2】

**位置：** `confined.rs` 的 Windows `open_root_handle`、`open_child_dir`、`open_existing`、`open_staged_for_cleanup`、`open_or_create_regular_file`。[S-Confined](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-workspace/src/confined.rs)

多处顺序是取得 raw HANDLE → `check_not_reparse(handle, ...)?` → 转成 OwnedHandle/File。若检查因 reparse point 或查询信息失败而返回 Err，所有权尚未交给 RAII，而检查函数本身不负责 CloseHandle。

这是错误返回路径上的资源泄漏，不是“reparse 检查允许了路径逃逸”：拒绝仍然发生。恢复专用 helper 已有先接管句柄、再检查的正确次序，应复用。OwnedHandle 的 drop 负责 CloseHandle。[E-OwnedHandle](https://doc.rust-lang.org/std/os/windows/io/struct.OwnedHandle.html)

Windows 故障/句柄计数测试未执行。建议重复触发拒绝与信息查询错误，观察句柄数量稳定，并验证正常文件仍可打开。

## 9. PROVIDER-02：Chat 的 length 终止丢失了输出上限语义【P2】

**位置：** `sse.rs::StreamAccumulator`、`lib.rs`、`responses.rs`。[S-SSE](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/sse.rs)[S-Provider](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/lib.rs)[S-Responses](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/responses.rs)

Chat 路径把任意非空 finish_reason 用于 seal，但仅对 network_error 等设置终止错误。如果收到 `finish_reason=length` 后又收到 `[DONE]`，纯文本或恰好可解析的工具 JSON 可以正常 finalize，length 原因未进入 provider-neutral 结果。

Responses 路径已把 incomplete 的 max_output_tokens 归为 OutputLimit，并映射为 `AgentError::ModelOutputLimit`。因此同一 runtime 根据适配协议不同会失去或保留“不完整”信号。

不要混淆：当前 Chat 确实要求 `[DONE]`；缺结束标记会报错，畸形工具 JSON 也会被拒绝。本项不是撤销这些已存在的正确保护。

建议统一两协议的终止类别，但不把输出上限直接改成无条件重试。已暴露的文本、已形成的工具结果与尚未执行的工具建议，需要按现有 Runtime 契约处理。补相同语义的两协议成对 fixture，本轮未执行。

## 10. PROVIDER-03：EOF 尾帧跳过 event/type 一致性检查【P2】

**位置：** `lib.rs::complete_responses_stream` 的正常 frame 分支与 `framer.finish()` 分支；`sse.rs::validate_sse_event_routing`。[S-Provider](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/lib.rs)[S-SSE](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/sse.rs)

正常空行结束的事件会 parse JSON，再检查 SSE event 名与 JSON type 是否一致，最后 apply。连接 EOF 的尾帧路径 parse 后直接 apply，没有同样检查。

可构造的最小静态反例：

```text
event: response.failed
data: {"type":"response.completed","response":{}}
```

正常空行结束会因 routing 矛盾拒绝；相同内容通过尾帧 flush 时可能进入 completed。事件内容不应仅因为最后有没有空行而改变校验强度。这是协议一致性问题，不是已经证明的 Core 权限绕过。

建议正常/EOF 两条分支使用同一个经过验证的事件 handler，补结尾格式变体测试。本轮未运行 Rust fixture。

## 11. MCP-01：取消未覆盖写阶段，部分错误返回未等待 reap【P2】

**位置：** `agent-capability-process/src/mcp.rs::request_with_cancel`、`send_frame` 与 lazy connect。[S-MCP](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-capability-process/src/mcp.rs)

请求开始前检查 cancel，但随后写请求只由 `timeout_at(deadline, send_frame(...))` 包裹，没有同时选择 cancellation。读取回应阶段才进入 biased cancel select。对端不再读管道、请求写阻塞时，取消可能一直等到写完成或原始请求 deadline，而不是走读阶段的及时取消路径。

`send_frame` 的写失败会 poison 并发起 kill，但 write 阶段的 `Ok(result) => result?` 可以在 await reap 前返回。后续 reconnect、stop 或 drop 仍可能清理，所以不能把它说成永久活进程必然泄漏。整个 Runtime 外层也可能 abort 工作；本项确认的是 client/adapter 边界没有履行其自身完整的取消/清理承诺。

建议让写、连接、读及锁等待的取消语义一致。写出过一部分后取消必须 poison session，不能把半帧通道当作健康连接复用；错误返回前的清理结果应明确。复用现有 deadline 和 supervisor，不新增通用 Scheduler。

## 12. PROCESS-02：reap 没有确认退出也清空 pid【P2】

**位置：** `ProcessSupervisor::reap`、`kill_tree`、`host.rs::kill_process_tree`。[S-Supervisor](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-process/src/supervisor.rs)[S-ProcessHost](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-process/src/host.rs)

reap 在持有 child mutex 时做第一次有界 wait；超时后 kill_tree，再做第二次有界 wait，但第二次结果被忽略，末尾无条件把 pid 置 0。第一次 wait 的内部 I/O 错误也没有被作为未清理处理。

Unix 非进程组首领的 fallback 用 `try_lock(child)` 获取直接子进程来 start_kill；此时 reap 正持有该锁，fallback 可能拿不到。生产 Unix host 通常设置 process_group(0)，正常路径受到这个前提保护；公开 from_child 与异常条件仍需要明确约束。

不能在未确认退出时把监督身份清空，并据此让 Drop 认为无需再处理。反过来，也不能永远保存一个可能已复用的裸 PID。建议返回 typed cleanup outcome，保留 owned child / 真实进程身份的清理责任，只有确认终态才清空。Windows 已持有 Job 时可能有额外 drop 清理，不能忽略这种平台差异。

本项未做仓库故障测试。测试需覆盖非 group leader、wait 错误、kill 失败和第二次 wait 超时，而非只测默认 sleep 正常退出。

## 13. CONTEXT-01：先截旧前缀再排序，newest-first 实际不成立【P2】

**位置：** `context-simple/src/index/dependency.rs::push_linked` 与 `index/indexes.rs`。[S-Dependency](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/index/dependency.rs)[S-Indexes](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/index/indexes.rs)

实体桶插入时按创建顺序追加。依赖建边先取每实体桶最前面最多 64 个不同 id，之后才按 slot 逆序排序，再选最多 8 个。若同一实体有 128 条正常创建顺序的 live 记录，选出的会是第 64 至 57 条，而不是第 128 至 121 条。若前缀大多是 dead，后面明明有 live 候选，也可能最后没有边。

这是一项确定的顺序/截断错误。代码注释称保持旧的逆向 heap newest-first 行为，但 cap 已经先把新记录丢掉了。`update_entities` 的 swap_remove 又会破坏桶顺序，所以仅改成 `.rev()` 不是对所有索引变更都正确的修复。

应在截断前确定合格候选与顺序，或维护可按创建身份取得 top-k 的索引。扫描工作预算与候选数量上限分别计数；不要让为“有界”增加的前缀截断悄悄改变语义。

**影响边界：** 这里建立 SharesEntities 关联边；materializer 只有 requires_prompt_body 的边才拉取依赖正文。因此，不能直接把本项升级为“GC 把必需正文删除了”或“错误建立了任务权威”。它首先影响关联图的质量及后续可解释性。[S-Materializer](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/materializer.rs)

反例由控制流和简单次序推导；本轮没有运行 Rust 测试。修复应加 64/65/128 条、dead 前缀、实体重索引和多个重叠桶测试。

## 14. DOC-01：活动文档仍把已落地事项写成待办【P3】

**位置：** `docs/CURRENT.md`、`scripts/doc_consistency.py`、`docs/EXECUTION_MODEL.md`。[S-Current](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/docs/CURRENT.md)[S-DocCheck](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/scripts/doc_consistency.py)[S-Execution](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/docs/EXECUTION_MODEL.md)

CURRENT 一部分已经记录较新的产品变化，末尾却仍把 CI 文档一致性检查与 EXECUTION_MODEL 提取列为待办；固定提交中这两项已存在，提交标题也说明已落地。

当前文档检查验证 JSON 必需字段是否存在、部分 report/window 路径、有限的历史短语黑名单、指定 11 个文档的相对链接，以及 toolchain pin。它没有逐项验证待办断言，也没有由状态对象生成全部活动文字。因此“文档 CI 绿”不代表活动路线在语义上全部一致。

最小改动是清理当前执行指令与已落地清单的矛盾，给历史 basis 明确日期；不需要再建一个更大的文档治理平台。也不应要求一个文件写入它自身所属 commit SHA 并永远等于 HEAD，这会产生自引用提交问题。可以记录最后验证的代码基线，明确它不是当前文件自动追踪的 HEAD。

INSTALL、COMPATIBILITY、RECOVERY_RUNBOOK 本轮取得全文，但只是审查声明及关联，不代表所有生产者/消费者和全部恢复不变量已完成一致性验证。

## 15. 上轮问题如何处理

以下属于上一份报告的延续，不计入新增 13 项，也不因本轮远端 CI 绿就判定修复：消费更新置于 debug_assert；PromptRequired 重复选择/扣费；关闭 stdout/stderr 后在 select 分支内无保护 wait；TUI 重建 run/水位混淆；run-summary 误读 required misses；Shadow Frame 混淆去重与限额省略；片段级正文身份不足。

本轮重新读取的 `tools/process.rs` 仍包含输出通道关闭后在分支内 `child.wait().await` 的路径；materializer 相关普通选择段也仍可见上轮判重问题的代码形态。[S-ToolProcess](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/tools/process.rs)[S-Materializer](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/materializer.rs) 对其余项目这里只保留上一轮结论，不声称本轮又完整追完了所有调用链。

目前源码 SHA 未变，上一轮待合并的 NEXT_TASKS 文件不能自动当成仓库里已经存在的文件。本报告不另生成一份竞争路线，避免 Coding Agent 同时接收多套“当前命令”。

## 16. 架构与路线影响

### 应保留的边界

现有文件显示 Runtime/Task/Turn、Core operation/effect authority、Context 工作集、进程与协议适配的职责分界仍值得保留。Core 普通 WAL 失败会 fence；增量 frame 读取会在追加前检查大小；MCP 读阶段有通知洪水上限与身份校验；Provider 正常主路拒绝缺终止标记和坏工具 JSON。这些是真实已有保护，不应在新审查中被忽略。[S-CoreOperation](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-core/src/operation.rs)[S-Frame](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-process/src/frame.rs)[S-MCP](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-capability-process/src/mcp.rs)[S-Provider](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/lib.rs)[S-SSE](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/sse.rs)

但不能再用“底层骨架大体完整”推导“底层失败语义已经闭环”。这轮发现集中在状态切换边缘：发布前/发布后、正常返回/错误返回、取消/硬崩溃、构建成功/实际复制产物、候选上限/排序语义。应沿这些边界做局部修复和测试。

### 近期建议顺序（不是额外总门禁）

| 工作包 | 修改范围 | 同现有产品路线关系 |
|---|---|---|
| A：恢复和进程责任 | STORAGE-01/02、PROCESS-01；同时修上轮 EOF 等待 | 受影响的恢复/验证能力不能在这些问题未处置时宣称闭环；F1/F2 的界面接线可并行 |
| B：发布与错误路径 | PACKAGE-01、WORKSPACE-01、PROVIDER-01 | 本地 Agent 的可安装、可启动、有限资源路径；不建设新平台 |
| C：适配一致性 | HANDLE、Chat 终止、EOF routing、MCP 取消、reap | 按模块原地修复，并把清理结果交给既有 Actor |
| D：核心资产质量 | CONTEXT-01、已有消费 ACK/片段身份/选择去重 | 先修事实与顺序，再比较新评分/缓存算法 |
| E：活动文档 | CURRENT 与实际文件对齐；旧研究命令退出当前队列 | 不重开 M15，不移动或改写旧 evidence 来掩盖新问题 |

Agent 功能主线仍建议保留续跑、补充输入、短计划/阻塞视图、有界执行和审阅。不能因为本轮又发现底层缺陷，就重新要求先建 Chronicle、TaskGraph、worker、多 Agent 才提供 /continue。

### Context、GC、搜索算法的本轮增量判断

本轮没有完成新的 BM25/SIEVE/TinyLFU 评测，也没有新的长任务数据，不能把此前提案升级为已验证推荐。本轮算法层唯一新增的确定性证据是依赖候选先截断后排序。

下一版先保证：候选宇宙与最终判断一致；排序前的上限不会系统性偏向旧数据；最终渲染/消费的正文身份准确；终态语义不受缓存热度改写。在此基础上再用现有基线比较词法排序、边际 token 装配与可逆正文缓存。通用 Scheduler 不是这些修复的必要条件。

## 17. 尚未完成的全仓范围

这轮源码正文阅读涉及 8 个 crate：agent-workspace、agent-storage、agent-core、agent-process、agent-capability-process、provider-openai、context-simple、tool-runtime。另有 11 个 crate 本轮没有新增正文审查：agent-compose、agent-conformance、agent-context-service、agent-contracts、agent-eval、agent-platform-protocol、agent-replay、agent-runtime、agent-tui、context-baselines、context-contextcore。部分以前看过，但不能因此计作本轮完整阅读。

即便已涉及的 8 个 crate，也有很多文件未取得正文。例如 provider 的 retry/wire_names 这轮未完整复读，process host 只读指定段，Context 的 store/checkpoint/GC 算法没有全部重新遍历。评测 evidence、历史 docs、所有测试 fixture、Cargo.lock 依赖审查和外部依赖漏洞检查也没有完成。

完成用户原要求还需要取得固定提交的完整 tracked tree，逐文件列明“已审查/生成材料/证据资产/未审查”，并在有 Rust 工具链的环境执行对应构建与测试。源码 ZIP 能消除当前正文获取障碍，但不会自动提供 Rust 工具链，也不会自动使测试通过。

**本报告的有效用途是可定位的修复输入和已执行探针记录，不是全仓安全、正确性或发布认证。**

## 18. 来源与交付物说明

下列仓库链接均绑定本次固定提交；不依赖 main 后续漂移。函数定位和已请求行范围见 READ_COVERAGE.csv。连接器把正文包在 JSON 字段中，聊天引用的 L2 是返回载体行，不应当作源文件第 2 行。


### 仓库来源索引

- S-CI：[.github/workflows/ci.yml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/.github/workflows/ci.yml)
- S-PackageWorkflow：[.github/workflows/package.yml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/.github/workflows/package.yml)
- S-Storage：[crates/agent-storage/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-storage/src/lib.rs)
- S-CoreOperation：[crates/agent-core/src/operation.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-core/src/operation.rs)
- S-CoreKernel：[crates/agent-core/src/kernel/mod.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-core/src/kernel/mod.rs)
- S-ProofRunner：[crates/tool-runtime/src/proof_runner.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/proof_runner.rs)
- S-ToolProcess：[crates/tool-runtime/src/tools/process.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/tool-runtime/src/tools/process.rs)
- S-Confined：[crates/agent-workspace/src/confined.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-workspace/src/confined.rs)
- S-RuntimeFacts：[crates/agent-workspace/src/runtime_facts.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-workspace/src/runtime_facts.rs)
- S-DistSh：[scripts/dist.sh](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/scripts/dist.sh)
- S-DistPs：[scripts/dist.ps1](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/scripts/dist.ps1)
- S-Provider：[crates/provider-openai/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/lib.rs)
- S-SSE：[crates/provider-openai/src/sse.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/sse.rs)
- S-Responses：[crates/provider-openai/src/responses.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/provider-openai/src/responses.rs)
- S-MCP：[crates/agent-capability-process/src/mcp.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-capability-process/src/mcp.rs)
- S-Supervisor：[crates/agent-process/src/supervisor.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-process/src/supervisor.rs)
- S-ProcessHost：[crates/agent-process/src/host.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-process/src/host.rs)
- S-Dependency：[crates/context-simple/src/index/dependency.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/index/dependency.rs)
- S-Indexes：[crates/context-simple/src/index/indexes.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/index/indexes.rs)
- S-Materializer：[crates/context-simple/src/materializer.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/context-simple/src/materializer.rs)
- S-Current：[docs/CURRENT.md](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/docs/CURRENT.md)
- S-DocCheck：[scripts/doc_consistency.py](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/scripts/doc_consistency.py)
- S-Execution：[docs/EXECUTION_MODEL.md](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/docs/EXECUTION_MODEL.md)
- S-Frame：[crates/agent-process/src/frame.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/12c86283b8d5991e9f17a07f14871dcf39d65066/crates/agent-process/src/frame.rs)

### 外部校验材料

仅用于平台/API 行为核实，不用于代替仓库事实。

- E-POSIX：[POSIX open（FIFO / O_NONBLOCK）](https://pubs.opengroup.org/onlinepubs/9799919799/functions/open.html)
- E-OwnedHandle：[Rust OwnedHandle](https://doc.rust-lang.org/std/os/windows/io/struct.OwnedHandle.html)
- E-Reqwest：[reqwest Response](https://docs.rs/reqwest/latest/reqwest/struct.Response.html)
- E-PowerShell：[PowerShell preference variables](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_preference_variables?view=powershell-7.5)

### 本地交付文件

- [FINDINGS.json](FINDINGS.json)：13 个新增问题与状态。
- [READ_COVERAGE.csv](READ_COVERAGE.csv)：本轮 29 路径的正文范围。
- [CUMULATIVE_READ_COVERAGE.csv](CUMULATIVE_READ_COVERAGE.csv)：两份现有台账合并，非完整仓库清单。
- [AUDIT_STATUS.json](AUDIT_STATUS.json)：机器可读范围与限制。
- [REMOTE_CI_OBSERVED.json](REMOTE_CI_OBSERVED.json)：远端 CI 状态整理。
- [TEST_MATRIX.md](TEST_MATRIX.md)：建议的局部回归，明确未执行。
- [probes/fifo_open_probe.json](probes/fifo_open_probe.json)：FIFO 机制实测。
- [probes/host_crash_probe.json](probes/host_crash_probe.json)：硬退出机制实测。
- [probes/dist_script_probe.json](probes/dist_script_probe.json)：原脚本 + 桩构建器实测。
