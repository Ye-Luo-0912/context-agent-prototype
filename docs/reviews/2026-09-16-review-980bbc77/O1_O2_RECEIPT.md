# 回执 — 遗留观察项 O1 与 O2

O1 出自 [6afa25df 审查](../2026-09-16-review-6afa25df/REVIEW.md) O1 节，
O2 出自 [980bbc77 审查](REVIEW.md) O2 节；两批回执均如实记录为「后续小切
片」。执行日期：2026-09-17。基线：main `71f8a586`（干净工作树）。提交：
O2 `3ed3e7ac`、O1 `9e9a8700`。执行方式：两个并行子代理，主代理独立复验。

## O1 — 资源采样：结构覆盖与数值完整性分离（.NET）——已关闭（`9e9a8700`）

- `SafeWorkingSet` 改 `TryReadWorkingSet`（失败不再产出 0）；`Parent` 改
  三态 `ParentRead`（Unsupported/FromPpid/Failure）——Linux 读/解析失败
  不再静默变 null（顺带移除多余 `Process.GetProcessById`，裸 ppid 更诚
  实：父已退出仍是已知关系）。
- 新 `ProcessRead`/`BuildGraph`：逐进程读取分类成图——未读工作集**排除
  并计数**（`WorkingSetReadFailures`，不再编码为 0）；父读取失败或进程
  消失 → 结构标签降级 `Unknown`。
- peak/idle/final 各自保留**实际所用样本**的 coverage 与读取失败数（旧
  代码 peak 只存数值、idle 的 coverage 整个丢弃）；报告新增 5 个可空字
  段（`peak_tree_coverage`、`peak/idle/final_working_set_read_failures`、
  `idle_tree_coverage`，NOT_RUN 时 JSON 缺失，旧消费者可读）；
  `tree_coverage` 文档明说 = 最后样本（final）的 coverage。
- 回归：红相（注入部分父关系失败：旧代码 `Expected: Unknown / Actual:
  FullTree`）→ 5 条新测试＋1 条扩展；**变异**（去掉父失败降级判定）→
  红 → 还原 → 绿。
- 验收：`dotnet test` 全套件 **139/139**；两工程 build 0 警告（
  TreatWarningsAsErrors）。
- 取舍：工作集读取失败不降级结构标签（父图完整则 `FullTree` 属实），
  缺口由计数器披露——消费者须同时读两组字段；peak 的质量取首个达到峰
  值的样本；未做平台资源实验、不声称实测内存收益。

## O2 — B2 认领读取的并发硬界（Rust）——已关闭（`3ed3e7ac`）

- 新 helper `store::read_existing_card_bounded(path, expected_len)`：打开
  句柄 → 句柄级 `is_file`（目录占位在句柄上被拒，不依赖读错误）→
  `take(expected_len + 1)` 有界读（分配容量也只按 expected_len+1）。字节
  上界是**结构性的**（take 落在句柄上），pathname metadata 不再是 read
  上限的假设。`run_external_spill_io` 的认领校验改走该 helper；fail-
  closed 语义不变（一致→认领；不一致/超长/不可读→原子修复写入；写入
  失败保持内联）。
- 回归：既有 B2 两条红线保持绿；新增 2 条——**有界可直接断言**（计划
  字节＋16 MiB 后缀：实际读到恰 expected_len+1 字节，非文件全长
  16,777,234）＋引擎行为面守卫（超长同名文件不可认领、修复后路径上是
  计划字节）。**变异**：take 破坏回无界整读 → 有界断言红（`left:
  16777234, right: 19`）→ 还原 → 全绿。
- 验收：`cargo test -p context-simple` **441/0**；clippy 0、fmt clean。
- 取舍：放弃「metadata 长度不等即不读」快捷路径——长度不等的文件现付
  一次至多 expected_len+1 字节的有界读（卡片字节受
  `external_checkpoint_card_bytes` 预算约束），换取上界结构性成立。竞态
  本身无法确定性构造测试：测试证明的是结构后置条件，不冒充已测竞态。

## 状态

至此 6afa25df 与 980bbc77 两轮审查的全部可本地执行项（QA/QB/QC/QD、
E1–E4、C 续本地阶段、O1/O2）均已关闭。剩余：T8 条件任务（真实端点接
受/缓存命中/净费用，需授权预算与凭据，无则保持 NOT_RUN），以及 E3 的
Linux 分片实际运行与 E4 fifo 测试的 Linux 执行——归 CI run 证明。
