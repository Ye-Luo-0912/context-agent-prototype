# 下一动作 — 以 d3a05d29 为基线

不修改未提交的他人工作；应用前核对实际HEAD与已合入修复。本文件是实施任务，不是已完成回执。

## C0：先调查实际CI，其他线不必等待

失败：Windows `named_pipe_t7_same_task_full_backend_journey`在纠正前没有等到part_a.md效果。
保留磁盘和同任务身份断言，补最小事件链定位；不要假定旧proof树问题又出现。
停止条件：能解释缺失发生在哪一步，定向反例通过，并完成本来失败的原旅程。
证据：[CI_OBSERVATION.md](CI_OBSERVATION.md)。

## B线 H1/H2：语义终结与冷热位置无关

**用户动作：**否定/保留指令不误删仍有效要求；明确替代在热页和冷页产生一致结果。
入口：`context-simple/src/gc/reachability.rs`、engine ingress/hydration、现有索引和生命周期提交。

实施顺序：先补don't/don’t的最小真调用链回归并修保守拒绝；再建立有界精确替代目标/待提交语义意图，补五位置等价性。
严禁：无限hydrate/pin、将Core anchor改成可被启发式重写、无证据放宽终态校验。
停止：保留/撤销正负对照通过；冷页变化跨restore不复活旧状态；权限、身份、资源上限保持。

## A/B线 H3/H5：真实源结束和真实文本交付

**用户动作：**合法最大工件能有限读完；中文错误/路径完整到达模型。
入口：`tool-runtime/src/tools/artifact.rs`、`page.rs`、`stream.rs`，最后经过真实broker与模型请求。

H3独立提交EOF修复；H5独立提交增量解码，避免把扫描和流处理合成一个巨型通用类。
必须覆盖cap−1/cap/cap+1及多字节跨块，检查source interval与raw bytes，不只检查单个sentinel。
停止：真实EOF而非预算猜测；合法UTF-8无伪替换；原始工件不变；背压、取消和capture上限不退化。

## 平台存储 H4：拒绝阶段决定恢复许可

**用户动作：**尚未写盘的容量拒绝可执行压缩补救，实际损坏仍禁止继续追加。
入口：`agent-storage/src/lib.rs`的append preflight/commit/fence与既有compactor。

新增有界测试额度/写入故障接缝，不改变production门限追求绿测试。
停止：no-write拒绝WAL字节不变且可压缩；partial-write和sync失败仍fenced；历史格式、稳定writer锁保持。

## C线：扩展已有KV生产轨迹，不建立新框架

现有产物已包含真实续读、完整前缀与Unknown计数。下一片补**有值缓存读写**、**实际触发维护**、**失败/重试累计**，检查每attempt和每lane的归属。
本地合成数据必须标SYNTHETIC_USAGE，不能冒充供应商成本。真实端点接受/命中/金额按已有授权与预算条件，未执行保持NOT_RUN。
停止：同一真实任务产物和语义完整性不下降，账目可复算；不要为缓存稳定延迟用户指令或保留失效规则。

## 建议命令（本环境未运行）

```sh
# 先用 --list 确认仓库测试名；下列为现存包的定向集合，不执行ignored付费测试。
cargo test -p context-simple --lib
cargo test -p tool-runtime --lib tools::artifact
cargo test -p tool-runtime --lib tools::stream
cargo test -p agent-storage --lib
cargo test -p agent-compose --test kv_production_sequence
cargo fmt --all -- --check
cargo clippy -p context-simple -p tool-runtime -p agent-storage --all-targets -- -D warnings
```

新增回归应给明确测试名并在回执中记录SHA、命令、结果和未执行项。完整CI在集成时跑；不要求每个局部变更重复付费长任务。
GUI继续后置；TUI只接可信公共结果，不再复制解码、owner或费用权威。
