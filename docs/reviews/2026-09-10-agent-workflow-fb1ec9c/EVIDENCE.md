# 执行证据

基线 `fb1ec9c069fea05911fa360408c5b117fad90219`，Windows / PowerShell，2026-09-10。本轮源码核对、所有以下反例和文档汇总由主审查者完成。未切换轻量模型；用户提出减少子 Agent 后，没有恢复或新建子任务。

## 可复现工件

- [独立 Cargo manifest](evidence/workflow-probe/Cargo.toml)，不加入生产 workspace。
- [反例源码](evidence/workflow-probe/src/main.rs)。Context、RuntimeHandle、ExecutionState、Workspace、BuiltinToolDispatcher 调用真实仓库库。
- [build.rs](evidence/workflow-probe/build.rs) 从当前 `actor/model.rs` 精确提取最终 packing helper，只将两个函数可见性改为 pub；其余逻辑不改。它验证 helper 的组合行为，不冒充完整 provider 预算测试。
- `ModelBackedCompactor` 通过 Rust path module 编译当前生产源文件；注入的是本地脚本 transport，不访问网络。
- [源码身份清单](source-manifest.json) 在交付时捕获相关生产源和反例源的 SHA-256。运行前后均核对 HEAD/工作树；它不是全部文件逐行已审的证明。

运行 cwd 为仓库根。基础命令：

```powershell
cargo run --offline --manifest-path docs/reviews/2026-09-10-agent-workflow-fb1ec9c/evidence/workflow-probe/Cargo.toml --target-dir target/review-2026-09-10 --quiet
```

此命令初次编译因探针没有 unwrap `BuiltinToolDispatcher::new` 的 Result 失败；修正后实际退出 0，执行 packing、九域证据、Storage GC、维护调用数、压缩失败与大工件读取六组探针。

之后只运行受新增/修正影响的入口，没有反复重跑全仓检查：

```text
同一 cargo run 命令末尾加：
-- followup       # 强化旧快照场景、Actor 维护取消、patch 候选
-- patch          # 单独验证 patch 候选
-- continue       # 两轮实际模型请求的完整指令
-- packing        # 两个必需区间实际选取最大项、移除、记录缺失
-- verification   # 使用规范 digest 的九域证据与补跑循环
```

`followup` 当时前两项完成，第三项因为探针没有提供 effect recovery identity 而退出 1；不是 GC/取消反例失败。patch 加入结构化 identity 后，首个失败 needle 没有产生候选，断言失败；改用“第一行匹配、后一行不存在”的真实多行 hunk 后，`-- patch` 退出 0。该调用必然在 staging 前拒绝，probe 未提交写入。`-- continue`、`-- packing`、`-- verification` 均退出 0。当前源码已包含这些夹具修正。

## 实际观察

以下为工具输出的字段摘录；省去随机 UUID、生产路径已有的 `DBG assembled input.tool_schemas=[]` 调试行和长 metadata。**探针断言现有问题可复现，退出 0 不表示生产缺陷已修复。**

| 问题 | 实际观察 |
|---|---|
| W01 继续指令 | `continue_result=Ok`；`request_count=2`；`marker_visible_each_request=[true,false]`；`raw_context_keeps_marker=true` |
| W02 最终曝光 | `required_body_present=false`；`required_misses=0`。最终夹具使用两个都必需的区间，真实 helper 选择并移除较大的一个 |
| W03 恢复根 | `protected_root_count=1`；`reconcile_deleted=0`；`blob_before_storage_gc=true`；`storage_gc_deleted=1`；`blob_after_storage_gc=false`；`retained_checkpoint_still_references_id=true`；`restored_live_body_available=false` |
| W04 调用数 | `old_input_chars=200000`；一次 maintain `compactor_calls=132`；脚本 usage 累计 `charged_input_tokens=66000`、`charged_output_tokens=16896` |
| W04 取消 | `cancel_reply_delayed_until_maintenance_released=true`；`submit_ok=true`；释放屏障后 `cancel_after_release=Ok(Cancelled{...})`；`stop_ok=true`，Actor JoinHandle 已结束 |
| W05 验收容量 | `allowed_criteria=32`；`allowed_domains=16`；`retained_facts=8`；`validity=Current`；`workspace_revision=0`；`directive_revision=0`；第一次当前域 `[1,2,3,4,5,6,7,8]`，补跑域 0 后为 `[0,2,3,4,5,6,7,8]` |
| W06 工件 | `artifact_bytes=3000000`；`end_line=200` 与 `end_line=1` 均拒绝：`artifact is 2097153 bytes; larger artifacts cannot be read in full (use a narrower range or a specialized tool)` |
| W07 patch | `ok=false`；`file_unchanged=true`；`candidate_mentions_uncommitted_text=true`；候选含 `let answer = NEVER_COMMITTED_VALUE;`，revision 仍是原磁盘内容的 digest |
| W08 失败压缩 | `model_calls=1`；`marker_attempted=true`；`archived=1`；`marker_retained=false` |

W03 的初始夹具在旧快照中保存 terminal 条目，随后强化为旧快照拥有 **Live、当时可 fetch 的正文**，当前状态才变为 terminal/aged。最终结论采用强化后的 followup 输出。状态变化通过合法 checkpoint 夹具注入，未声称做了真实宿主任务完成/断电/冷启动全链。

W04 的 250 ms 是确定性屏障下的观测窗口，用来证明 cancel 回执依赖维护释放；不是平台 p95 延迟或 SLA 结果。132 次调用是实际脚本调用次数；token 数由脚本输出，不是真实 provider 用量或费用。

W05 原始 attribution 的 class identity 使用短字符串，后改为 SHA-256 digest 后单独复验，结果相同。证明的是当前证据账本容量及查询行为；完整 acceptance gate 依赖由当前源码核对，没有让真实模型完成一个九域任务。

## 未执行与工程边界

- 没有真实 provider、验证进程、生产工作区文件修改、GUI 实操、跨平台测试、打包或远端 CI 验证。
- 未运行全仓 cargo test、冻结 M15/LT-EVAL 或完整现有 crate 回归；本轮是审查，不是功能实现验收。
- 所有实验文件位于 tempfile 的自有目录；patch 拒绝后检查真实文件未变；Actor 取消实验明确 stop 并等待任务结束。未读取/更改用户 `.trae/`。
- `.NET`/平台上轮修复不是本轮重点，未声称重新验收。恢复根读取失败等补充静态候选不计入八项动态发现。
- 文档/链接/源码清单检查只验证交付物一致性，不证明产品问题已解决。最终结果追加在下方。

## 最终交付检查

已执行独立探针的 `cargo fmt --manifest-path .../workflow-probe/Cargo.toml`，仅格式化本轮探针；未格式化生产树。现有文档检查：

```text
C:/Users/Ye_Luo/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/python.exe scripts/doc_consistency.py
document-consistency gate: OK (13 live docs, links and state agree)
```

退出 0。另检查新报告的 8 个本地链接，全部存在；保存 32 个相关生产源、4 个探针源/manifest 的 SHA-256，检查结果保存于本目录 verification.json。HEAD 仍为本轮基线；`git diff --stat` / `git diff --check` 无输出；status 仅有用户原有 `.trae/` 与本次新增审查目录。未跟踪的新文档不包含在 tracked diff 检查中。
