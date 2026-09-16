# 980bbc77 可执行补充任务

基线 `980bbc77f4086ebf8848f5c9afa16ce22fafe39f`。这是当前后端阶段的补充切片，不是新的大阶段。先核对实际工作树/HEAD/未提交改动，再按文件所有权处理；不得覆盖并行工作。旧 QA/QB/QC/QD 已有实现，不按旧缺陷全文重做。

## A/B：E1 输出裁剪与投影覆盖（先行）

用户动作：Agent 读取较大的源码片段后，仍能辨别哪些正文真正处于本次输入，缺失中部不会被历史去重隐藏。

入口：agent-workspace/broker.rs、agent-runtime/output.rs、prompt.rs、必要的现有 ToolOutput/窗口契约。共享契约由一个集成人维护。

先写真实反例：历史保存中部短窗口，后续同版本全读超过输出上限，实际经纪去掉中部；组装最终输入，原实现不得把 sentinel 报成完整覆盖。另测 Runtime 兜底、正常未截断读、相同版本不相交窗口。

修改：任何可信正文转换必须同时更新/失效投影覆盖，源版本仍按原身份；metadata 与总预算在更新后再次满足约束。不得解析截断提示文字来决定可信性，不以 artifact_ref 存在替代全文可见，不全禁用正确去重。

顺手补同一 helper 的 output_budget=0/1 等小预算；marker 本身也应纳入上限。到投影完整性与预算回归通过即停止，不扩大成重写提示词系统。

建议定向验证（本轮未执行）：
```sh
cargo test -p agent-workspace --lib
cargo test -p agent-runtime --lib
cargo test -p agent-compose
```
新增精准过滤命令前先用 `-- --list` 确认测试存在；0 tests 不是绿灯。集成反例必须走实际输出经纪，不只是手工写 `window_truncated=true` 的 DTO。

## Core/Contracts：E2 参数语义与摘要域

用户动作：两个实际可区分的长整数参数不会被当作同一个 operation 参数身份。

入口：schema_profile.rs、jcs.rs、operation.rs，以及 Core 准入/派发的共享参数验证入口。

先固定反例：n=9007199254740992 与 n=9007199254740993，普通 integer profile 均可接受，原始 as_i64 不同但 hash 前 f64 相同。测试明确它是规范化碰撞，不是密码学碰撞。

决定并文档化数值域：拒绝非无损整数或使用明确安全子集；需要精确长整数则经版本化 schema 使用字符串。归一化方案必须使授权、摘要、执行消费同一语义值。约束值 minimum/maximum/enum 同样遵守该域。

不要新增 `expect` 把合法 serde_json 值变成进程 panic；不要直接改变现有持久摘要读法，先保留旧兼容或设计迁移。常规 1/1.0 等价、对象键排序、历史向量必须按既有契约保持。

建议验证：
```sh
cargo test -p agent-contracts
cargo test -p agent-core
cargo test -p agent-platform-protocol
```
Core 本身仍有权限/意图检查，不得把本次修复解释成绕过或删除它们。

## 验证设施：E3 + E4（可并行的小切片）

E3：将 agent-host 纳入一个 Linux Rust 测试分片，并确认所需 sibling binary fixture 构建。用 workspace metadata 与 CI 预期包集合做轻量一致性断言；显式排除有名称和原因，不静默漏包。沿现有 CI，不增加另一个测试平台。

E4：用 CString + OsStrExt 改正 runtime_facts.rs 内 mkfifo 的测试参数；保持分配存活。核对创建的文件类型是 FIFO、路径就是预期路径；然后运行原不阻塞回归。不能用过去 CI 偶然成功证明 unsafe 前置条件成立。

建议 Linux 验证：
```sh
cargo test -p agent-host
cargo test -p agent-workspace project_markers_do_not_block_on_a_writerless_fifo -- --nocapture
```
滤波匹配数必须非零；没有 Miri/ASan 不阻塞这个一处 CString 修正，也不能声称已完成 sanitizer 全量检查。

## C（续）：实际请求序列的供应商 KV 与任务总成本

保留现有 CurrentStateLast、状态后置、typed split、稳定路由键、显式断点与 attempt 用量结算。当前动作不是再加一个缓存开关，而是复用现有 HTTP 捕获设施走连续真实请求。

固定同一任务、工具契约、provider profile 和预算。依次引入：状态计数变动、合法新证据、相同版本不同窗口、文件修改、工具撤销、协议 checkpoint、取消后迟到用量。对比最终 wire 的前缀/边界 digest、首差异类别和正文完整性。E1 的大文件裁剪场景必须纳入，防止“少发必要证据”成为虚假降本。

本地阶段证明 layout/wire/账目语义；真实供应商阶段另外证明 accepted/hit/收费。按主调用所有 attempts + 维护所有 attempts + 适用缓存写入/读取/存储统计，未知保持未知，任务质量相同才比较净收益。绝不为缓存保留过期正文或失效权限。

没有真实端点预算时，本地序列照常完成，真实端点部分维持 NOT_RUN。GUI 不扩展。

## 停止条件与文档写法

E1 的真实跨层反例、E2 的域一致性反例、Linux 包集合和正确 FIFO fixture 完成后继续主体任务；不要求所有非阻塞观察清零。阶段收尾用既有跨进程同任务旅程 + 实际任务质量/成本记录。

CURRENT/NEXT_TASKS 只写状态/下一动作和本报告链接，关闭项详细过程放回执。CI 结果必须绑定实际 SHA、run、attempt；当前本报告只确认该 SHA 的 run `35149164719` 最后查询仍进行中。
