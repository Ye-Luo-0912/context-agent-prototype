# 当前阶段补充实施任务（c8a62355）

目标：保留上轮已合入成果，沿现有后端模块修正 owner、交付游标、WAL 写者与 Windows containment 边界。详见 [REVIEW.md](REVIEW.md)。本文件为任务规格，**不是已经执行的补丁或验证回执**。

## 接手与并行边界

先检查分支、HEAD、工作树和在途修改；不要覆盖其他开发者的未提交代码。当前报告 SHA 是 `c8a623554f762505061cdfdd7466ccaa995a3de0`，实现前按实际 HEAD 对照，但不把新版本结果回填成旧 SHA 已通过。A=执行/工具/TUI，B=Context/GC/搜索，C=平台/供应商 KV/成本。共享契约由一名集成人维护。

B 的 G1、A 的 G2/G3 可先并行；C 的 G4 与 A 的 G5 根据文件所有权并行。KV 验收改动沿现有 agent-compose tests，避免 provider 反向依赖 Runtime。GUI 只做被协议变化要求的兼容，不加功能。

## G1 — 逻辑 owner 驱动 reconcile（B，优先）

**用户动作：**保持固定小热预算，冷历史仍可检索／取回；恢复后 reconcile 不把现存冷 owner 当作孤儿，也不把全部历史拉回热表。

**入口：**context-simple/engine.rs::reconcile_store_protecting、store.rs::run_reconcile_io_protecting/commit_reconcile、现有目录查询与 metadata-residency 操作。

先建 fixture：至少 1 个 hot、2 个 pending，全部合法 blob/card，热上限固定为 1，确认本次 hydration 真的因预算／HotCap 未加载目标。调用 protecting reconcile；检查所有逻辑 ID 恰有一份 owner，各驻留集合互斥，pending 卡片身份没被 blob 状态覆盖。加入真正孤儿正对照。

实现完整 owner 快照与提交复核。未读冷卡片有定位即不是孤儿；需要详情时目标化解析。合法 orphan 接纳后走现有预算结算或明确背压。重复 reconcile、checkpoint/restore，再取回正文。

**停止条件：**上述反例与正对照通过，现有保护根／删除延期行为不回退。不得全历史 hydration、无限 pin 或放宽 owner validator。

## G2/G3 — 最终模型预算内的连续交付（A/B，同一个工具所有者）

**用户动作：**原样使用工具给出的 continuation，能读完原始工件，不跳过中间行／行内后缀，不把第 3 行显示成第 2 行。

**入口：**tool-runtime/tools/artifact.rs、agent-workspace/broker.rs；必要时 Runtime output.rs 与共享输出契约，但避免把工具页字段补丁散落在多处。

先写经过真实 broker 的两组反例：500 行约 75.5 KB 的普通日志（每页中段有独立 marker）；3 MiB 单行（1 MiB、2 MiB、2.5 MiB 等位置有唯一块 ID）。从工具实际返回参数继续，合并最终显示的源区间／块 ID，必须无遗漏、无重复，end 只能在完整交付后出现。

再写 UTF-8 边界：余量只剩 1 字节，下一行以“界”开始，后面有 ASCII 短行。不能跳过前行继续接纳后行；验证 source line identity、继续位置、末页状态。

实现以最终 model-content budget 切页并预留 footer/编号，或在裁剪器中维护可验证的源区间映射与游标重写。扫描／capture／最终交付位置分开，不能把捕获位置写成已交付位置。保留上轮动态 end_line、行内偏移、不可变引用和 confinement。

**停止条件：**正常短页不退化；不同预算/长行/多字节字符都能按返回游标收敛；最终包络不超限；原工件中段仍可达。只增加 window_truncated 标志不算完成。

## G4 — journal 生命周期锁（C，存储所有者）

**范围：**库级 FileOperationJournal；正常 Workspace 外层锁已提供保护，本片不得宣称在生产必现或移除该锁。

用确定性 barrier 暂停 B 于读取旧 metadata／打开旧 WAL 后、取锁前；A 完成 compaction；再放行 B。检查 B 不能作为旧代健康 writer 返回。单独令下一代候选文件已被活动 writer 锁定，竞争 compaction 拒绝时文件内容/长度必须不变。

实现读 metadata 前取得且跨代持有的不轮换锁；候选文件安全创建，不对现存活动路径先 truncate 再取锁。明确多进程与同进程独立句柄规则，稳定 lock 文件不能在持有期间被 unlink 重建。保留旧 WAL/metadata 格式及恢复兼容。

**停止条件：**旧代开锁、候选写入、普通双开和故障恢复测试通过；Unix/Windows 各运行本平台语义，不用 Linux flock 机制探针冒充 Rust/Windows 结果。

## G5 — C0 同类 Windows 入口收敛（A，共用现有进程机制）

C0 proof runner 的挂起创建修复已存在，不重新实现一份。核对 generic ProcessHost 与 Low-IL run_wrap，消除 spawn 后建立必需 Job 的窗口；明确处理 assign_pid_to_job 的失败。只有成功关联／恢复后的受控 child 可发布给调用方，失败回收未运行 child。

公共机制应只负责创建、关联、恢复、生命周期；Core 审批/EffectIntent 不搬进 runner。attestation 来自实际成功证据。已有外层 Job 的保护与内层 Job 的必要性应明确，不因内层失败就猜测整个进程树必然失控。

用可控屏障、立即派生后代、强制关联失败和宿主死亡回归覆盖每个真实生产入口。

**停止条件：**已确认必需 containment 在目标代码运行前建立；失败不留下可运行孤儿；原 proof 路径继续通过。本轮没有 Windows 执行结果。

## C（续）— 固定生产请求序列，不扩张评测平台

保留 kv_production_sequence.rs 的真实磁盘断言、同任务 key、工具加载/撤销、恢复与费用脚本测试。

补全每轮整个稳定前缀及 tools 的摘要/字节比较；不能仅比 B0 所在一项。加入 G2/G3 的最终正文集合检查。费用记录扩展到供应商读写桶、实际尝试、维护调用和未知项，且同一累计 snapshot 不重复加。捕获服务器脚本值只证明传输与结算，不表示真实缓存收益。

真实供应商参数接受、缓存命中与净金额对照均为条件实验；需要明确预算和凭据，没有则 NOT_RUN，不影响本地修复。

## 建议定向命令（均未在审查环境执行）

```bash
# 在实际 Rust 工作树中执行；先让新增回归在旧行为下失败，再修复。
cargo test -p context-simple --lib reconcile
cargo test -p tool-runtime --lib artifact
cargo test -p agent-workspace --lib broker
cargo test -p agent-storage --lib
cargo test -p agent-process
cargo test -p agent-capability-process
cargo test -p agent-compose --test proof_supervision
cargo test -p agent-compose --test kv_production_sequence
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
# 最终仍用既有 CI 分片；Windows containment 在 Windows runner 执行。
```

## 文档与停止规则

每片回执记录 SHA、真实入口、反例、正对照、平台、已执行/未执行项和残余。当前任务只保留下一动作和回执链接，不再次追加全部审查历史。不要把“有字段”“内部 cap”“单个 sentinel”“获得某一代锁”当作整条不变量的证明。
