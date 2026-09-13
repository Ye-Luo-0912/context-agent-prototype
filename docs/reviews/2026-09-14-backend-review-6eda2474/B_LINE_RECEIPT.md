# B 线回执：N01 / N02 / N03（基线 `f4155b53` → `3537d6c5`）

日期：2026-09-14。实施：ZCode B 线会话（共享树内并行有 A/C 线未提交 WIP，本切片只提交 B 线文件）。

## 固定版本

- 起点基线：`f4155b53`（审查包入库后的 main，CI run `34770832962` 全绿）。
- 提交：`3537d6c5`（仅 6 个 B 线文件；A/C 线 working-tree 改动未触碰、未提交）。

## 改变的真实用户动作 / 故障

- **N01（P1）**：之前——已含外置元数据卡片（分片 checkpoint）的持久 store 在服务冷启动时，启动 reconcile 用「空保护根＋`roots_complete=true`」清扫：blob 被保守重认领，其**卡片却被当孤儿删除**；`roots_complete=false` 的保护性调用同样保卡不力。恢复时只能缺失/降级，checkpoint 捕获的元数据版本丢失。现在——卡片清扫与 blob 清扫**共用同一删除许可**：本次扫描重认领（rebuilt）的 id 卡片保留；根枚举不完整时一律延期；两个生产启动缝（`agent-context-service` 二进制、compose 组合根）显式以「根集合不完整」做**非破坏性 reconcile**（只重认领、不删除）。真孤儿（blob 与 owner 均消失）在完整根下照常清理。
- **N02（P1）**：之前——`hydrate_pending_cards` / `hydrate_card_for` 先把 pending 行移出队列再 await 磁盘读取：future 在读取中被丢弃（取消）即丢失目录项；所有读取失败统一计为 missing 并消费 locator。现在——锁内只**复制**有界读取计划，pending owner 原位保留；锁外读取；重拿锁后仅对「同 id＋同 hash 且仍由 pending 持有」的验证成功条目提交迁移。typed `ExternalCardRead::{Found, Missing, Corrupt, IoFailed}`：Missing/Corrupt 消费并如实计数（新增 `external_card_io_failures` 与 missing 分列）；IoFailed 保留可重试 locator（restore 把瞬态失败行送回 pending，manifest 照常携带）；批量排水遇纯瞬态失败即停，不死循环。
- **N03（P2）**：卡片读取复用 blob 的有界读模式（打开句柄→metadata 上限→`take(cap+1)`），超大卡片在读取处拒绝；按 manifest 捕获的 12 位内容哈希核对（同 FNV 损坏检测器，**非密码学认证**——回执如实声明）；延后页安装前做与 `checkpoint::validate` 同型的 scope 结构验证，非法页不改变运行状态、行被消费计数、卡片文件留在磁盘可诊断。

## 生产调用链

- 删除许可：`store.rs::run_reconcile_io_protecting` 卡片清扫段 → `engine.rs::reconcile_store_protecting`（actor/R3-01 透传）→ 服务进程启动（`main.rs`）与 compose 组合根（`lib.rs::compose_with_prompt_layout`）。
- 读取链：`store.rs::read_external_card_checked_async` ← `engine.rs::read_card_with_test_hooks` ← `hydrate_pending_cards` / `hydrate_card_for`（fetch_external 路径）← `restore` 首批读取。
- 兼容：全部为既有行为的收紧，无 wire/契约格式变更；`external_card_io_failures` 为 `#[serde(default)]` 加法字段，旧 checkpoint 解析不变。

## 实际执行的验证（本机，Rust 1.97.1）

- `cargo test -p context-simple --lib` → **395 passed / 0 failed**（新增 5＋重写 1 项回归，见下）。
- `cargo test -p agent-context-service --test service` → **17 passed / 0 failed**（新增真实服务进程重启回归）。
- `cargo test -p context-baselines` → 25 passed；`cargo test -p context-contextcore` → 7 passed；`cargo test -p agent-compose --lib` → 39 passed。
- `cargo clippy -p context-simple -p context-contextcore -p agent-context-service -p agent-runtime --all-targets` → 0 警告；相关文件 `cargo fmt --check` 干净。
- **行为级红检查（真实执行，非叙述）**：
  1. 恢复「先移除再读」旧行为 → 两个取消回归转红（批量：pending 从 2 变 0）；
  2. 禁用哈希核对 → 哈希失配回归转红；
  3. 禁用 scope 结构验证 → 未知 scope 回归转红；
  4. **联合红检查**：同时退回「启动参数」与「cards 分支」两个修复 → 真实服务重启回归在「启动 reconcile 不得删除保留 checkpoint 的卡片」处转红（证明审查指出的「只改启动参数不够」）。
- 远端 CI：run `34774960364`（`3537d6c5`+fmt `7cac88b7`）**七 job 全绿**——首次 run 的 Windows full 败于已记载的 `named_pipe_stop_is_bounded_with_no_client` 宿主 e2e 满载抖动（非本切片产品测试），`--failed` 重跑通过；fmt 首跑失败为本切片文件格式遗漏（本地 check 输出被截断误判已净），`7cac88b7` fmt-only 修复。

## 新增/变更回归清单

1. `reconcile_cleans_orphan_cards_and_honors_protection`（重写：真孤儿清扫 8 ＋ protected 1 ＋ rebuilt 卡片随 blob 幸存）。
2. `an_incomplete_root_enumeration_defers_card_deletion`（新：不完整根延期删除；完整根重认领后卡片随行）。
3. `a_cancelled_batch_hydration_keeps_every_pending_row`（门控暂停＋abort，逐行保留）。
4. `a_cancelled_id_fetch_keeps_its_pending_row`（单条 fetch 取消后重试可取回正文）。
5. `a_transient_card_read_failure_keeps_the_retryable_locator`（瞬态 I/O 与 missing 分列，恢复后恰一 owner）。
6. `an_oversized_card_is_refused_at_the_bounded_read`（超限→Corrupt；缺卡→Missing）。
7. `a_card_whose_bytes_lost_the_captured_hash_is_corrupt_and_never_installs`（同 id 不同捕获字节永不安装）。
8. `a_deferred_card_referencing_an_unknown_scope_never_installs`（延后页结构验证；文件留盘可诊断）。
9. `a_service_restart_over_one_store_keeps_the_sharded_checkpoints_cards`（真实服务进程：外置→分片 checkpoint→退出→同 store 重启→restore→捕获元数据逐字段一致＋正文可取）。

## 未执行 / 边界

- 真实供应商调用照旧 NOT_RUN；付费实验不启。
- 全仓 `cargo test --workspace` 未跑（CI 矩阵覆盖）；Windows full 以 CI 为准。
- `hydrate_all_pending_cards` 的总工作量边界（审查「规模与配置残余」节）属 B3 后续，沿原队列，不在本片冒充完成。
- N04–N10 归 A/C 线；`cache_wire_flow.rs` 等共享树内未提交 WIP 未触碰。

## 仍未完成的接线

- A 线（session 事实/轮询预算/grace/MCP 分页）与 C 线（生产填 key 的端到端 wire 验收——共享树中 C 线 WIP 进行中）不在本回执范围。
