# PKG-H1：公共 host 长正文解码修复

2026-09-20，本地 Windows；基线 `71322074` 之上的未提交工作树实现。
状态：**本片本地验收通过**。原 package v2 应用实验仍为未验收，冻结证据不回写。

## 用户行为与修复

此前超过 4096 个 UTF-8 字节的 `work.submit` 或 `work.steer` 文本会在 host
JSON 解码时关闭连接，尚未进入正文契约或 Runtime 接纳检查。
现在 `agent-host/src/lib.rs::decode_budget` 仅将单字符串和累计字符串预算设为
已有的 `MAX_FRAME_BYTES`（1 MiB），其余沿用 `JsonDecodeBudget::control_plane()`。

保留的限制：帧 1 MiB；嵌套深度 16、数组长度 64、对象键数 64、节点数 512；
正文契约 200000 字符与 256 KiB UTF-8。未改通用 JSON 默认预算、Runtime 调度、
TaskAnchor、Core 权限或完成语义。

**现有业务限制单列**：新持久任务的 `work.submit` 仍受 TaskManager 的
2000 字符目标上限，返回 `Domain/work.rejected`。TUI `/work` 和 `--work` 也走
同一路径；本片没有改写这项身份规则。不能靠截断显式新任务目标消除限制，
因为 `prepare_create` 依据目标文本相等恢复任务，会混淆相同前缀的不同长目标。
长 `work.steer` 使用当前任务的完整指令路径。

## 行为验证

1. 新 decoder 单测修复前 **2 失败、1 通过**：合法的 200000 字节输入、
   应到达正文超限检查的 200001 字节输入，都提前报 `StringBytes { max: 4096 }`。
2. 真实 Named Pipe 新回归修复前在长 submit 上报 `connection closed before response`。
   应用解码修复后首次运行又显露既有 2000 字符任务目标限制；测试保留该拒绝，
   没有扩大任务锚点上限或把拒绝当作接纳成功。
3. 最终真实 E2E：约 1900 个汉字的目标成功提交，详情与留存正文身份完整；
   20 KB ASCII、24 KB UTF-8 纠正应用到正确 TaskId，完整字节数及 SHA-256 一致。
   超任务上限返回 Domain 错误；超正文字符/字节上限返回关联的
   `Protocol/protocol.request_invalid`。每次拒绝后同一连接仍可读取，快照与指令不变。
4. decoder 单测另覆盖 CJK 接近 256 KiB、四字节字符恰好 256 KiB、
   超字符/字节限额、深层/宽数组/宽对象/节点超量、畸形 JSON 和超大帧头拒绝。
5. 新构建的真实 host 二进制执行原 120 次重复的 **9512 字节纠正**：
   T1/T2/T1、错误任务拒绝、运行中纠正与满队拒绝、可见模型协议错误、
   checkpoint、冷恢复、取消与迟到回复隔离、4 个独立宿主进程均通过。
   供应商为本地合成端点，付费调用 **0**。
   [运行回执](host-text-fix/host-journey.json)、[源码及二进制摘要](host-text-fix/provenance.json)。

## 实际检查

| 命令 | 结果 |
|---|---|
| `cargo test -p agent-host --lib decode_tests:: -- --test-threads=1` | 修复后 3/0 |
| `cargo test -p agent-host --test host_e2e named_pipe_work_text_limits_preserve_connection -- --exact --nocapture` | 1/0 |
| `cargo test -p agent-host` | 共 35/0（lib 11、config 3、E2E 10、process variant 6、restore 3、T7 2） |
| `cargo clippy -p agent-host --all-targets -- -D warnings` | 通过 |
| `cargo fmt --all -- --check` | 通过 |
| `cargo build -p agent-host` | 通过；新二进制用于上述本地合成流程 |
| `python scripts/doc_consistency.py` | 通过 |
| `git diff --check` | 通过 |

合成流程实际命令：

```text
python scripts/package_endurance/host_journey.py target/public-host-text-fix-20260920/workspace target/public-host-text-fix-20260920/evidence --directive-repeats 120
```

生产检查范围是 host framing/解码/dispatch、正文验证、显式任务创建与完整指令留存
调用链的局部读取；不是全仓源码审查。新增 E2E 共用 Windows/Unix 流程，
本机执行了 Windows Named Pipe，Unix 端待对应 CI 环境执行。本轮未运行全 workspace
测试或远端 CI，未创建提交/PR。手工过度转义使编码后超过 1 MiB 的 JSON 帧仍会拒绝，
不承诺所有等价 JSON 编码均能发送。
