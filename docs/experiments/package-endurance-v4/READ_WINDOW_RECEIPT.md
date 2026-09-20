# fs.read 默认窗口修复回执

日期：2026-09-20。范围：v4 真实模型轨迹暴露的文件读取参数可用性；这是后续本地源码修复，不是已完成的真实供应商复测。

## 实际触发与修改

`model-finish-api` 第 5 轮仅传 `start_line=509`，旧 `end_line=200` 默认值使调用报 `invalid line range`。`model-repair-and-finish` 第 7 轮请求 118–520 共 403 行，被既有 400 行硬上限拒绝；当时提供给模型的 schema 没有说明这项跨度限制。

[FsReadTool](../../../crates/tool-runtime/src/tools/fs.rs) 的私有 `ReadArgs.end_line` 改为可省略；省略时通过 checked arithmetic 派生 `start_line + 199`，与同 crate 已有 `artifact.read` 的相对窗口语义一致。默认起点 1 仍得到 1–200，显式起止范围不变。

200 行默认窗口与 400 行最大跨度都来自既有常量。schema 的 `end_line` 描述说明相对默认值、包含端点的跨度上限以及 EOF/正文预算可能缩短返回页；工具描述把这两个数字放在前 90 字符内，避免描述缩短时丢失。

400 行硬上限、文件字节上限、最终正文分页、原始行号、全文件 revision、交付窗口与 coverage 规则保持不变；未变更共享 contracts 或增加新工具。

## 本地验证

平台：Windows。所有 Cargo 检查显式使用独立目录，未向正式 `target/debug` 产物写入：

```powershell
$env:CARGO_TARGET_DIR = 'target/v4-read-window-check'
```

| 命令 | 结果 |
| --- | --- |
| `cargo test -p tool-runtime --lib fs_read_default_window -- --test-threads 1`，实现修改前 | 2 个预期红例：broker 路径返回 `invalid line range`；schema 缺少默认/窗口声明。首次隔离编译 51.27s，测试 0.02s |
| `cargo test -p tool-runtime --lib fs_read -- --test-threads 1`，实现修改后 | 15 通过、0 失败；编译 6.09s，测试 1.10s |
| `cargo clippy -p tool-runtime --all-targets -- -D warnings` | 通过，31.52s |
| `rustfmt --edition 2024 --check crates/tool-runtime/src/tools/fs.rs` | 通过 |
| `git diff --check` | 通过 |

新增回归走真实 `FsReadTool → WorkspaceOutputBroker`，核对从 509 开始的默认 200 行窗口、EOF clamp、默认第一页、显式范围对照及全文件 revision。边界回归证明 400 行仍接受、401/403 行仍拒绝，倒序、零起点、默认终点溢出均拒绝。原有大文件、超长单行、UTF-8/换行、分页续读与交付覆盖相关回归一并通过。

本机完整日志在 `target/v4-read-window-receipts/red.log`、`green.log`、`clippy.log`；源码与正式二进制摘要在同目录 `identity.json`。这些是本地验证产物，不是远端 CI 回执。

## 产物身份与限制

修改后的 `fs.rs` SHA256：

```text
c44dbb1e8244d5374cf76cd9d6549739bef0ffe0d5d6f46e30d598555f5d60ae
```

正式二进制在本次工作前后摘要一致：

| 文件 | SHA256 |
| --- | --- |
| `target/debug/agent-tui.exe` | `7213b5989599a8daf016fa363f8eafa933658741b6c7a2274c1c16b6688ddbe8` |
| `target/debug/agent-host.exe` | `8fe08822dc9208e5bf4a40af3d8d276c07c574a9e6c27e3c59b29211a0f9f923` |

v4 已记录及当时仍在执行的付费轨迹使用旧二进制；不能将本次源码修复写成真实任务质量已经改善或供应商复测已通过。本次没有追加付费请求、没有改候选应用或预算，也没有改写冻结轨迹证据。未运行全 workspace 测试，Unix 与远端 CI 尚待其各自验证。
