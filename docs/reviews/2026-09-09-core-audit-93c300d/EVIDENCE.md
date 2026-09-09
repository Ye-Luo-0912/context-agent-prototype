# 本轮执行证据与限制

审查基线：`93c300d9b222ea9720579b86ac273e945f1964bc`，日期 2026-09-09。初始、期间多次核对 HEAD 相同，tracked diff 为空；已有 `.trae/` 未跟踪目录未读取、未修改。

## 证据保存事件

最初各探针、报告和目录清单均写在忽略目录 `target/review-2026-09-09/`。汇总期间两次现场检查发现整个 target 已不在磁盘，原因未确认。HEAD 和生产源码 tracked diff 未变化。原始清单不能再与最终清单逐项对照，原 Core/.NET/IO 探针不能再从该路径直接重跑。

本目录的 CORE_RUNTIME/PLATFORM/IO_TOOLS 文档据本轮实际工具输出重建，清楚保留了实验范围和失败结果。Context 探针源码依据本轮已读取的源码重建到本目录，并重新实际执行，见下。重建报告与重跑实验是两个不同事实。

[source-manifest.json](source-manifest.json) 是汇总时新捕获的 tracked source/build SHA-256 清单（485 个文件），包含所用筛选规则匹配的 Rust/C#/Python/shell/build 文件；不是“每份文件逐行已审”的证明，也不是原已缺失清单的复制件。

## 现存可运行的 Context 反例

源码：[context-probe/src/main.rs](evidence/context-probe/src/main.rs)，[Cargo.toml](evidence/context-probe/Cargo.toml)。它是独立 workspace，不加入生产 Cargo workspace。所有磁盘数据来自 tempfile 自有目录。

执行命令，cwd 为仓库根：

```text
cargo run --offline --manifest-path docs/reviews/2026-09-09-core-audit-93c300d/evidence/context-probe/Cargo.toml --target-dir target/review-2026-09-09/context-build --quiet
```

重新编译后退出码 0；输出如下。探针断言的是**现有错误仍可复现**，不是修复完成或产品验收通过：

```jsonl
{"case":"rolling_source_coverage","compactor_calls":1,"constraint_retained_in_checkpoint":false,"constraint_sent_to_compactor":false,"default_config":true,"folded_records":1,"source_characters":[2000]}
{"case":"pending_owner_storage_roots","heap_owner_deleted":0,"live_pending_owners":1,"pending_owner_deleted":1,"referenced_evidence_still_exists":false}
{"case":"pending_only_gc_retry","catalog_rows":0,"externalized":0,"pending_remaining":1}
{"case":"retained_checkpoint_after_reconcile","deleted_as_stale":1,"older_checkpoint_body_available":false}
{"body_visible_with_same_revision_other_window_hint":false,"case":"disjoint_file_body_coverage","control_body_visible":true,"hinted_bodies":["src/a.rs@rev-1"]}
```

随后把 range 夹具补成与 metadata 一致的实际 100 行正文，并加入独立 `range` 入口；只重跑受影响的 range 反例：

```text
cargo run --offline --manifest-path docs/reviews/2026-09-09-core-audit-93c300d/evidence/context-probe/Cargo.toml --target-dir target/review-2026-09-09/context-build --quiet -- range
```

退出码 0，最后一行输出与上面相同。未更改前三项反例逻辑。

## 其他实际执行

| 实验 | 实际结果 | 证据范围 |
|---|---|---|
| Core checkpoint debt 私有 Actor 探针 | 编译成功，安全断言失败；0 passed / 1 failed | [CORE_RUNTIME.md](CORE_RUNTIME.md)，不是完整磁盘崩溃恢复；原源码已缺失 |
| .NET wire / ViewModel / paused dispatcher 探针 | dotnet run 退出码 0；四类反例成立 | [PLATFORM.md](PLATFORM.md)，scripted peer，非真实 Rust/provider；原源码已缺失 |
| IO/tools Windows 与 WSL 原生 Linux 探针 | 锁超期、槽泄漏、poll 取消不兑现、游标重复均复现 | [IO_TOOLS.md](IO_TOOLS.md)，真实库及自有子进程/文件；原源码已缺失 |
| Linux change journal 重定向 | 外部自有文件 113 → 478 字节；读到唯一 marker | IO_TOOLS 附录；Windows link 权限失败、before-open 场景 NOT_RUN |

IO 的第一次 WSL `/mnt/d` 运行因原子替换后目标验证失败而停止；改用原生 `/tmp` 自有数据目录后才完成探针。未将挂载差异另立产品缺陷。测试用子进程均在当时以正式 stop/OS 检查确认退出；未清理用户目录。

本轮未运行全仓 cargo test、完整 .NET 回归、真实 provider、冻结 M15/LT-EVAL、打包发布或远端 CI；现有测试被阅读不等于本轮运行。旧文档里的 CI 结果不外推到本基线。

文档检查已执行：

```text
C:/Users/Ye_Luo/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/python.exe scripts/doc_consistency.py
document-consistency gate: OK (13 live docs, links and state agree)
```

退出码 0。另以 Python 检查本目录 43 个本地 markdown 链接全部存在，并按最终 manifest 重新计算 485 个 source/build 文件 SHA-256，全部一致；结果保存为本目录 verification.json。该核对只对最终重新生成的 manifest 成立，不能替代与已缺失初始 manifest 的对比。

最终 `git rev-parse HEAD` 仍为基线；`git diff --stat` 和 `git diff --check` 无输出，status 只有用户已有 `.trae/` 与本次新增审查目录。对独立探针运行了 `cargo fmt --manifest-path docs/reviews/2026-09-09-core-audit-93c300d/evidence/context-probe/Cargo.toml`，只格式化探针源码；未再扩展测试。以上检查不验证工程问题已经修复，tracked diff 检查也不包括未跟踪的审查目录。
