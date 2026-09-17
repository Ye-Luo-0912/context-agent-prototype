# G4 回执 — operation journal 生命周期锁

提交：`b0652cde`。任务规格：[NEXT_ACTIONS.md](NEXT_ACTIONS.md) G4；缺陷分析：[REVIEW.md](REVIEW.md) 第 5 节。

## 范围声明（保持）

库级 `agent-storage::FileOperationJournal` 自身；正常独立 `Workspace::open` 的外层 effect-journal 锁（`agent-workspace/src/journal.rs`）**未动、未削弱**，本片不宣称绕过它或生产必现。审查环境的 Linux flock 机制实验不冒充 Rust/Windows 结果；本片全部执行验证在 Windows 本机完成。

## 实现（规则落在代码所在地）

- **写者独占绑定不随代际轮换的 journal 身份**：稳定 lock 文件 `<base>.lock`（如 `operations.lock`），`open_journal_lock` 以非截断方式创建，**在任何 journal 真相（metadata、WAL 代际、遗留候选）被读取之前**取得，存于 `_journal_lock: File` 字段持有整个生命周期；不轮换、持有期间不被 unlink 重建。锁取得后的任何拒绝都会 drop 句柄释放锁。
- **候选代际安全创建**（`open_compaction_candidate`）：全新候选走 `create_new`＋立即取锁；已存在候选（只可能是崩溃后未发布的压缩遗留——调用方持生命周期锁且 metadata 未发布）以**不截断**方式打开、先取锁、再 `set_len(0)` 重写。旧的 create+truncate→写入→才 try_lock 流程移除。
- 每代 WAL 自身的锁保留为纵深防御（现场注明）；metadata/remnants/missing-WAL 拒绝顺序与首个 WAL 创建的目录 sync 链保持。
- **并发契约**（`FileOperationJournal` doc 注明）：跨进程与同进程独立句柄都经生命周期锁串行——flock 按打开文件描述（Unix）、LockFileEx 按句柄（Windows），同进程双句柄在两平台都冲突。Windows 同进程拒绝本机执行；Unix 镜像 `#[cfg(unix)]` 门控、本机未执行。

## 回归（红→绿）

- `a_stale_metadata_opener_is_serialized_by_the_journal_lifecycle_lock` — channel-barrier 交错：第二个 opener 在任何锁之前快照到 G1 metadata；第一实例压缩发布 G2 并存活。旧代码红（临时 cfg(test) stall hook 钉住"已读 metadata＋已开 WAL、未取锁"窗口，随修复移除）：`the stale opener came back as a healthy writer: healthy-stale-writer generation=1 …`——B 对已删除但仍打开的 G1 句柄取锁成功、按陈旧 metadata 返回健康写者（其后续压缩在写入时被 Windows ERROR_LOCK_VIOLATION 挡住，但 open 已成功）。修复后：opener 被类型化拒绝（`Storage` 指名 `operations.lock`），第一实例保持唯一健康写者，drop 后下一次 open 跟随已发布 G2（恢复 last_seq 3、2 ops）。
- `competing_compaction_is_rejected_without_touching_the_locked_candidate` — 外部句柄锁住哨兵填充的 `operations.jsonl.g2`；压缩类型化拒绝且候选**逐字节不变**（经持锁句柄读内容＋metadata 取长度），metadata 未发布、writer 保持在 G1 健康；外部锁释放后同路径压缩成功。旧代码红：哨兵被截断 24 字节→0（`left: []`）——先截断后拒锁。
- `journal_lifecycle_lock_is_stable_across_generations_and_double_open` — 锁文件首次 open 即存在、跨压缩存活、同进程二次 open 被类型化拒绝且锁文件不被移除（Windows 执行的正对照）。
- `compaction_reuses_an_unlocked_leftover_candidate_after_locking_it` — 崩溃压缩遗留候选：取锁→清空→重写；重开恢复合法 G2。
- `unix_same_process_double_open_is_rejected_by_the_lifecycle_lock`（`#[cfg(unix)]`，仅编译验证，未执行）。

最终提交的 stale-opener 契约测试对旧代码结构性绿（旧缺陷窗口在 `open()` 内部——审查因此只能外部验证 flock 机制）；该精确窗口的红由上述临时 hook 实验确定性建立并已记录，hook 不在提交内。

## 已执行验证（Windows，cargo 1.97.1，合树后集成复跑）

- `cargo test -p agent-storage`：**30 passed / 0 failed**（25 个既有恢复/格式/兼容测试保持绿＋5 新增）。
- `cargo test -p agent-workspace`：117＋5＋3 passed / 0 failed（journal 集成不回退）。
- `cargo check -p agent-storage --all-targets --target x86_64-unknown-linux-gnu`：干净（cfg(unix) 编译验证；**未执行**）。
- `cargo clippy -p agent-storage --all-targets -- -D warnings`：干净；fmt check 干净。
- 机会性核对：`agent-runtime --test turn completions_past_the_hot_window`（journal 重开重试路径）与 `--test instance` 31/0 通过。

## 边界与残余

- 跨进程独占依赖 OS 锁；同进程句柄冲突与跨进程是同一 LockFileEx/flock 机制，本机执行的是同进程变体。跨进程第二进程未在本机实测。
- Unix 的 flock 非阻塞取锁与截断危害写半边未在本机执行（与审查的 Linux 机制实验分别记账）。
- 旧代 WAL 删除在 Windows 上对 std 打开的外部句柄为 delete-pending 语义（名字消失、陈旧句柄存活至关闭）——生命周期锁下不再有 journal 写者持有该陈旧句柄；既有 `published_generation_survives_a_crash_before_old_wal_removal` 覆盖。
