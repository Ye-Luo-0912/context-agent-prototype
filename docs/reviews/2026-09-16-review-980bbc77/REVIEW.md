# 980bbc77 续审：输出证据完整性、参数语义与验证覆盖

## 基线与证据边界

仓库：`Ye-Luo-0912/context-agent-prototype`。固定 SHA：`980bbc77f4086ebf8848f5c9afa16ce22fafe39f`；相对上一轮 `6afa25df` 新增 6 个提交。提交时间为 2026-09-16 20:52:37 UTC（东京时间 2026-09-17 05:52:37）。收尾查询 main 仍为此 SHA。

对应 CI run `35149164719`，最后查询为 attempt 1、`in_progress`、conclusion=null。这里只记录该次查询结果，不借用其他提交的绿灯。

本轮核对全部 20 个 workspace crate 的结构/成员，并实际阅读 **23 个不同文件**的全文或表列区间。并未完成全部生产文件、全部测试、脚本、SDK 和 GUI 的逐行审查。目录枚举不是正文覆盖；搜索命中不是全文件阅读；未读区域没有“无问题”结论。详见 COVERAGE.md。

本环境未发现 cargo/rustc/dotnet；`git ls-remote` 因 `Could not resolve host: github.com` 失败。源码经 GitHub 连接器读取。本轮没有运行 Rust/.NET/PTY 回归，没有真实供应商调用，没有修改或推送仓库。附带 Python 机制检查已执行，仅验证局部数学/转换/集合关系，绝不等同于完整产品复现。

## 结论与优先级

| 标识 | 建议优先级 | 类型 | 结论 |
|---|---|---|---|
| E1 | P1 | 生产正文正确性 | 经纪/兜底截断后保留旧覆盖声明，使不完整 fs.read 可能充当完整窗口并省略历史正文。 |
| E2 | P2 | 公共参数身份契约 | 参数校验按精确整数，摘要按 binary64；存在不同可接受整数被摘要为相同字节的输入。 |
| E3 | P2 | CI 覆盖缺口 | Linux 两分片只选择 19 个 workspace 包，遗漏 agent-host Rust 测试。 |
| E4 | P2（测试设施） | unsafe 前置条件 | FIFO 测试把不保证 NUL 终止的 Vec 指针交给 mkfifo。 |

E1 最先修。E2 与 E3/E4 可以并行，C 线实际请求序列的 KV/成本工作继续推进。不存在本轮发现足以要求整体重写 Runtime/Context/平台的依据。

## E1：截断正文后，源文件覆盖声明仍被当作模型可见覆盖

### 源码链

1. [crates/tool-runtime/src/tools/fs.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/tool-runtime/src/tools/fs.rs) 的 `FsReadTool::execute` 对实际读取的文件生成 `start_line/end_line/covers_file` 与版本。每行可能很长；400 行不是 16,000 字符上限。
2. [crates/agent-workspace/src/broker.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-workspace/src/broker.rs) 的 `WorkspaceOutputBroker::bound` 把超限正文写为工件，并生成首尾预览；正文变了，正常大小的 metadata 没有获得 `truncated/window_truncated` 标记。
3. [crates/agent-core/src/authority.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-core/src/authority.rs) 的 OutputAuthority 调用该经纪，不重新计算文件可见窗口。
4. [crates/agent-runtime/src/output.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-runtime/src/output.rs) 的最后防线也会改正文而不更新窗口事实。
5. [crates/agent-runtime/src/prompt.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-runtime/src/prompt.rs) 的 `file_read_window_from_output` 仅用 metadata 中这两个截断布尔量决定 complete 与 covers_file。没有标记，就仍按完整结果处理。
6. 同文件 `omit_selected_file_body` 把这些窗口交给 `visible_body_windows_cover`，覆盖了同路径/版本/区间就省略历史正文。

这不是“版本旧了”，而是**版本身份正确，模型可见字节却不完整**。有完整工件只证明可以继续读取，不证明该工件正文已进入当前请求。

### 条件反例

先把某文件中部的短区间作为历史必要证据保存；再读取同一版本全 400 行，总正文超过经纪字符上限，中部正好被首尾裁剪去掉。最终请求若把新结果当作完整窗口，历史中部正文又因冗余被省略，则必要信息实际没有出现在输入中。

本轮 Python 机制检查构造了 34,825 字符输出，预览为 16,000 字符，中部 sentinel 不在预览中，而未修改的 metadata 仍推出 `complete=true/covers_file=true`。这不是实际运行 FsReadTool/Core/PromptAssembler 的集成测试；完整调用链回归仍未执行。

### 最小修复与维护性

在改变模型正文的同一个可信出口，重新派生/失效其**投影覆盖事实**。最低限度：正文裁剪发生时标记对应文件窗口不完整；更精细的方案可记录确实保留的独立窗口，但不能用首尾两段冒充连续中间区间。源版本、原读取范围与当前模型可见范围应区分。

经纪与 Runtime 兜底复用同一套更新规则。标记更新之后仍须保证 metadata 与总包络预算。不要靠解析提示文案判断可信完整性，也不要把所有带 artifact_ref 的输出都直接当作不完整：引用存在与否不是覆盖证明。

### 回归与停止条件

走真实读取→经纪→协议尾→最终装箱，断言中部 sentinel 真正存在，或缺口被诚实报告，绝不能报告“已有完整覆盖”。覆盖小窗口、完整文件、相同版本不相交窗口、metadata 截断、无工件失败路径、Runtime 兜底。正常未裁剪的窗口仍然允许正确去重。

修复到这些后置条件成立即停止，不借此改写整个提示词或 ContextEngine。

### 同一模块的小预算观察

`truncate_with_marker` 将保留正文预算饱和减到零，但仍总是放入完整 marker。Some(1) 这类公开允许的 output budget，返回值可能超过 1 字符。机制检查中是 77 字符。这个返回仍有固定较小上限，不是无界内存漏洞；随 E1 同一转换函数补 `0/1/marker_length±1` 边界即可。常规 16,000 字符配置不是这一小预算现象的触发条件。

## E2：参数规范化域与执行域不一致

### 已确认代码事实

[crates/agent-contracts/src/schema_profile.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-contracts/src/schema_profile.rs) 的 Integer 校验用 `Value::as_i64()`，没有把每个整数限定到可无损转为 binary64 的域。[crates/agent-contracts/src/jcs.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-contracts/src/jcs.rs) 的 `write_number` 则调用 `Number::as_f64()`，再序列化。

[crates/agent-contracts/src/operation.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-contracts/src/operation.rs) 的 `ArgumentDigest::from_json` 无条件哈希这份 JCS 字节，并假设任意 serde_json::Value 都已符合所需域。[crates/agent-core/src/kernel/mod.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-core/src/kernel/mod.rs) 用这个摘要比较工具准入、发布和执行的参数身份；执行请求仍携带原始 `call.arguments`。

例如：

```json
{"n":9007199254740992}
{"n":9007199254740993}
```

两个值均属于 i64 范围、可通过无额外上下界的 integer profile；它们转 binary64 后相同，因此规范化字节与摘要相同，但工具读取原始整数仍可区分。

**这不是 SHA-256 碰撞，也不是已执行证明的权限绕过。**它是摘要之前的有损转换，与精确参数语义不一致。单靠对象键排序和普通小整数样例不能覆盖它。

RFC 8785 §3.1 要求先将输入适配到 I-JSON 数值域；需要更长整数/更高精度的应用推荐使用字符串。按 binary64 序列化本身不是错误，错误在于另一条路径仍按未归一化的精确整数执行，却把摘要视为它的唯一身份。

### 修复选择

在共享参数准入处明确数值契约。可拒绝无法无损表示的整数，或明确采用更严格安全整数子集；需要精确长整数的字段使用有版本的字符串表示。若决定采用 binary64 语义，校验、授权、摘要、派发必须消费同一已规范化值，不能只在 hash 时悄悄舍入。

同时检查 schema 的 minimum/maximum/enum：整数上下限目前先经 f64 再转整数，不能让同类舍入进入约束本身。不要把修复做成 hash 中新增 panic；不应因调整新输入域让合法旧 checkpoint/WAL 在没有迁移方案时失效。artifact 的字节摘要不受此类语义规范化影响。

### 验收

核对 2^53 邻域、正负非精确整数、普通 1/1.0 等既有等价约定，以及跨 Rust/.NET 测试向量。每个实际允许执行的值，应与身份规范化域一致。至少在 SchemaProfile→ArgumentDigest→Core 准入链验证，而不是只测规范序列化 helper。

本轮只执行了 binary64 数学机制检查，没有编译并执行上述 Rust 反例。

## E3：Linux 测试分片遗漏 agent-host

[Cargo.toml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/Cargo.toml) 声明 20 个 workspace members。[.github/workflows/ci.yml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/.github/workflows/ci.yml) 中 Linux part 1 选择 7 个包，part 2 选择 12 个，两个集合无交集，合计 19 个，唯一遗漏 `agent-host`。

工作流中的 `cargo check -p agent-host --all-targets` 与 `cargo build --workspace` 只编译，不运行该包 Rust 回归。Ubuntu 的 .NET job 会构建宿主并运行自己的跨进程链路，这是实际覆盖，不能说 Linux 完全不运行宿主；但它不等于运行 `agent-host` 包内 Rust 的 Unix/UDS 用例。Windows full job 也不能代替 Unix 条件编译分支的运行覆盖。

最小动作：把 `-p agent-host` 加入适合的 Linux 分片（并核对 sibling fixture 构建要求），沿现有 CI 运行。增加一个很小的“预期包集合=分片并集+明确排除项”检查，防止新增包再次漏选。无需新建测试平台，也不要以把 job 命名为 full 代替实际选择。

这项说明的是验证覆盖缺口，不说明 agent-host 在 Linux 已经坏了，也不是当前 CI 进行中的失败诊断。

## E4：FIFO 测试的 C 字符串前置条件不成立

[crates/agent-workspace/src/runtime_facts.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/980bbc77f4086ebf8848f5c9afa16ce22fafe39f/crates/agent-workspace/src/runtime_facts.rs) 的 Unix 测试 `project_markers_do_not_block_on_a_writerless_fifo` 当前将：

```rust
let path = fifo.as_os_str().as_encoded_bytes().to_vec();
libc::mkfifo(path.as_ptr() as *const libc::c_char, 0o600)
```

交给 C API。Vec 中没有附加 NUL，不能保证是合法 C 字符串。偶然遇到邻接零字节并不满足安全前提；可能越界读取或形成不同的路径。这是**测试设施中的 unsafe 问题**，不能宣传成生产 Runtime 的远程漏洞，不能据此归因历史 CI 失败。

修复很小：在 Unix cfg 下用 `OsStrExt::as_bytes` 与 `CString::new` 构造有所有权且正确终止的路径，保持 CString 存活到调用返回；先确认创建的 fixture 真的是 FIFO，再运行不阻塞探测。无需引入新的 unsafe 封装库。

```rust
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
let path = CString::new(fifo.as_os_str().as_bytes()).expect("fixture path contains NUL");
let rc = unsafe { libc::mkfifo(path.as_ptr(), 0o600) };
assert_eq!(rc, 0);
```

本轮未执行 unsafe 反例；没有必要为了演示一个前置条件缺失而故意做越界读取。

## 已修复内容与未提升为缺陷的观察

- QA：生产代码中已看到 `pending_cold_items`、按本预览卡片版本的逻辑 owner 校验与冷消费记录路径。本轮不原样重开“冷 required 一到 ACK 就无 owner”。
- QB：Chat 读流由外层 seal 统一携带 attempt 已知用量；Responses 同类改动的完整测试结果仍依其回执，本轮不是两条协议全部分支的再执行。
- QC：新 SDK 将 `_eventsOverflowed` 纳入重连判定；同锁连接/队列安装、snapshot barrier 与旧 pump 入队检查已有实现。
- QD：新 TUI 将循环错误保存到 outcome，正常与异常退出进入共同 worker 收尾；计数改为预注册 ledger。
- 空命中检索：当前引擎在 cold coverage 不完整且 hits 为空时返回明确错误及续查提示，因此没有把 Core 的空结果展示直接判成可由当前引擎触发的虚假零结果。
- access stamp 保留旧卡片 claim 是代码明确承认的弱状态策略；本轮不把它单凭“卡片字节旧”就定性为权限/正文丢失。
- 原 O1 资源采样完整性、O2 已有卡片 opened-handle 读取仍按原队列处理，不换编号重复派工。

上述是静态核对范围内的结论，不等同于所有对应测试已由本轮执行。

## 下一阶段仍然推进后端主体与供应商 KV

当前 `PromptAssembler` 已把 attention/currentness 放到动态状态段，selected evidence 在前，foreground/misses/external/restored 不扩大已声明的稳定证据边界。应保留这些改进，不按旧版本再次派发。当前 `EvidenceSplit.epoch=0/1` 描述本次装配里的消息数，并不证明跨轮证据集合已经稳定。

因此 C（续）的下一动作应是固定真实任务轨迹，在最终 mapper 后比较请求序列：仅状态变化、新检索、正文版本变化、工具加载/撤销、checkpoint 恢复、相同窗口重新读取。记录变化发生在哪个边界、正文/描述符切换是否必要、工具 schema 是否稳定，以及输入/缓存读写/输出与维护和失败尝试的用量。

缓存 key 不替代前缀内容匹配；工件保存不等于当前模型看见；更短正文不自动等于更低任务费用。优先保持有效信息正确更新，随后消除无业务意义的前缀变化。E1 修复后的序列应进入此对照，不能把错误删除正文获得的低 token 当作收益。

真实端点接受、实际命中、净金额分别验收；无预算/凭据继续 NOT_RUN，不阻塞本地请求序列测试。GUI 继续后置，TUI 只做可信操作入口。不要新增调度器、第二份任务真相或独立大重构阶段。

## 文档与维护性约束

本轮建议集中四个维护入口：输出转换与投影事实、参数规范化与执行语义、workspace 包与测试分片、unsafe fixture 构造。职责划分比“拆更多文件”重要。

CURRENT/NEXT_TASKS 只替换当前状态和下一动作，链接本报告；不要把本文全文再追加进去。每个切片具备必要回归后继续主线，阶段收尾沿用既有 CI 与同任务真实流程，不以审查项永久归零为终点。

## 外部核对资料

- RFC 8785 §3.1/§5：<https://www.rfc-editor.org/rfc/rfc8785.html>
- Rust CString：<https://doc.rust-lang.org/std/ffi/struct.CString.html>
- Cargo test package selection：<https://doc.rust-lang.org/cargo/commands/cargo-test.html>
- OpenAI Prompt Caching：<https://developers.openai.com/api/docs/guides/prompt-caching/>

外部资料只支持其对应规范结论；仓库实现判断以固定 SHA 源码为准。
