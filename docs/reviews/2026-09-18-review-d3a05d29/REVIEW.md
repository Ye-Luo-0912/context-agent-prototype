# 后端续审：语义生命周期、读取终止、流解码与日志错误分层

**基线：`d3a05d297295da1ece66b00245fa027f2ee12852`**  
范围与验证：[COVERAGE.md](COVERAGE.md)；原始清单：[READING_MANIFEST.json](READING_MANIFEST.json)。

## 结论

先阻止否定用户指令误终结旧约束，并完成冷页上的相同语义规则；读取的终止条件、进程输出解码和WAL拒绝分类并行收口。
保留已有架构与前一批修复。GUI仍后置；TUI与SDK继续作为后台执行的操作入口，不能另立运行权威。

| ID | 建议级别 | 发现 | 证据层级 |
|---|---|---|---|
| C0 | 当前集成阻塞 | Windows同任务旅程未等到part_a.md落盘 | 远端CI日志已读；根因未定位 |
| H1 | P1 | don't/ don’t否定漏识别，旧决策可被错误Superseded | 静态调用链＋谓词机制移植 |
| H2 | P2 | supersession只覆盖已加载owner，冷页可错过语义终态 | 静态调用链；Rust反例未执行 |
| H3 | P2 | 恰好8 MiB被误认为扫描未完，EOF后继续返回续读 | 真实临时文件＋coverage公式移植 |
| H4 | P2 | 未写盘容量拒绝永久封禁writer，压缩也无法执行 | 静态调用链；Rust反例未执行 |
| H5 | P2 | 固定字节分片逐片lossy解码破坏合法UTF-8 | 真实字节边界解码机制检查 |

CI当前失败与H1–H5独立；参见[CI_OBSERVATION.md](CI_OBSERVATION.md)。未执行本地Rust/.NET/Windows回归；本包不修改仓库、不调用供应商。

## H1：否定形式被解释成显式撤销

### 源码

- [reachability.rs：分类与否定保护](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/reachability.rs#L1-L340)
- [reachability.rs：直接宾语及取代队列](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/reachability.rs#L340-L620)
- [终态应用](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/reachability.rs#L780-L1000)
- [用户消息进入](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/engine.rs#L2530-L2940)
- [minor维护](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/minor.rs#L1-L315)

`classify_decision`与replacement cue都会命中`remove `。`has_retention_protection`按空白分词，只裁词两端标点；`don't`内部的直撇号、`don’t`内部的弯撇号仍保留，不能匹配名单里的`dont`，也不是`do not`。
随后whole-entity规则把`remove AuthService.rs`作为撤销旧实体的证明。

同一任务先收到：`Use AuthService.rs with a 5-second timeout`；之后收到：`Don't remove AuthService.rs`。
共享实体、Decision分类、replacement cue和直接宾语均成立，旧行可被排入Superseded并由维护转为终态。旧行里的timeout等仍有效要求因此被一起排除。

**边界：**这影响启用该supersession路径的Simple/Dynamic Context，并不直接改写Core TaskAnchor，也不删除文件、绕过权限。机制检查没有实际启动ContextEngine。

### 最小修复和设计约束

对常用否定形式先采取保守保护，歧义时共存；不要将“某个撤销词＋共享实体”直接等同于终结旧规则的权威。
更稳定的责任边界是：候选相关性与明确的替代目标分开，能够证明精确旧记录/要求被撤销时才终结。避免以追加越来越多英文词表为长期方案，也不需要为此新建语义模型服务。

### 回归与停止条件

真实同TaskId的ingest→maintain→materialize→checkpoint/restore，覆盖直/弯撇号、Do not、Never、Keep和明确Remove正对照；旧timeout不得因否定句消失，真正明确撤销仍生效，跨任务不受影响。

## H2：逻辑owner统一了，语义更新仍然按驻留位置分歧

### 源码

- [supersession/error verification扫描位置](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/reachability.rs#L340-L620)
- [apply_terminal_semantic / drain](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/reachability.rs#L780-L1000)
- [定向hydrate入口与用户消息](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/engine.rs#L2530-L2940)
- [批量hydrate](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/engine.rs#L1570-L1830)
- [已修复的reconcile逻辑owner](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/engine.rs#L3270-L3455)

queue和apply的目标枚举覆盖Resident、Warm、pending_externalize_retry及已加载external，不覆盖pending_external_cards。
普通UserMessage/ToolObservation不会因为该判断而全量加载冷页；后续页安装的语义值来自卡片，未见对该次已经错过的替代指令进行重放。

条件反例：同一条旧Decision，放热目录时会被明确的`Remove AuthService.rs`取代；先降至未加载卡片后，同一句话扫描不到它。后续取回时旧记录仍可能是Live。
这与G1重复认领不同：唯一owner可以完全正确，而语义已过时。相似扫描结构也用于旧Error的verified-fixed，具体扩展需各自验证身份规则。

### 修复

生命周期操作面对逻辑条目，而不是当前内存位置。使用精确身份/版本的有界终结意图或现有目录的延迟更新；冷页发布之前应用已确认的终态。
不能证明时保留待解析状态与审计，不把未知ID当作不存在消费掉；不要求全历史同时驻留，也不无限pin。

### 回归

同一记录在五种位置具有相同语义结果；冷页变更→checkpoint→restore→fetch不恢复旧Live；跨任务、不同验证probe和不同版本不误终结。取消/暂时读取失败不丢未完成意图。保持热资源上限。

## H3：扫描预算耗尽与真实EOF混为一类

### 源码

- [artifact coverage及扫描循环](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/artifact.rs#L1-L645)
- [合法producer上限](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/stream.rs#L1-L230)

读取固定使用`file.take(MAX_SCAN_BYTES)`，扫描完成性只看`reader.get_ref().limit() > 0`。文件恰好8 MiB时，budget为0与真正EOF同时成立；当前公式只保留前者。
`coverage_facts`含`has_more = !scanned.complete || ...`，因此即使最后一页内容全部交付，也继续给下一范围。单行工件从末页之后可以得到201、401、601……，每次仍从起点读取相同前缀，无法建立真实结束。
producer的截取恰好停在8 MiB，因此不是“不受支持超大外部文件”的特例。

**边界：**不必然漏掉已交付正文；主要问题是错误的完成/续读语义与无效调用。大于预算的真实后缀不能因修复而被谎报EOF。

### 修复和回归

有界探测一个额外字节，或使用等价的真实EOF证明，区分BudgetStop和SourceEnd。继续位置必须绑定同一个不可变工件。
参数化小边界做单测，真实8 MiB做集成；cap−1、cap、cap+1，末尾有/无换行、多行/单行、原样continuation，验证内容无空洞且恰好cap有有限结束。
后续性能可沿已有定位减少每页从起点复扫；没有测量前不报告延迟收益。

## H4：容量拒绝尚未写盘，writer却被永久标记失败

### 源码

- [append_and_sync与compact](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/agent-storage/src/lib.rs#L575-L790)
- [append_operation_record前置边界](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/agent-storage/src/lib.rs#L940-L1110)

当前追加前检查完整帧和文件投影大小；超上限时尚未seek/write，原文件可保持完整。
上层却把任何append_operation_record错误统一写入writer.failed。之后compact/recover/marker等接口会拒绝这份writer；文件容量错误文字提示需要checkpoint/compaction，而同句柄已不能执行该补救。
自动压缩只由序号阈值触发，字节上限可以先到。实际产品是否到此取决于运行和额外压缩策略，本轮没有构造满额Rust WAL。

### 修复

区别RejectedBeforeWrite、NeedsCompaction与WriteOutcomeUncertain。前置合法拒绝不应与部分写入/flush失败采用同一个sticky fence。
可在已有字节阈值触发压缩并有限重试；压缩后仍容纳不下时明确拒绝，不循环。任何写入已开始或持久性无法确认仍必须封禁并走恢复。
不要回退已修的跨代稳定writer锁，也不要改变旧WAL格式。

### 回归

通过可注入小额度验证：拒绝前后WAL逐字节相同、原marker有效、同句柄仍可压缩；另注入半写/flush失败，确保fence不被放松。

## H5：字节分片上限不能作为UTF-8解码边界

### 源码

- [pump_stream / MAX_STREAM_ITEM_BYTES](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/stream.rs#L1-L230)
- [StreamCapture逐片from_utf8_lossy](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/stream.rs#L220-L420)

pump按4000原始字节切片，而record对每片独立lossy解码。`a×3999 + 界`是合法UTF-8；界的三个字节被分到两片，两片各自成为非法序列并被替换。
原始工件拼接仍然正确，但模型正文中的路径、诊断、标识符可以失真；这不是Ratatui渲染宽度问题，改TUI布局不能解决。

### 修复

保留原始字节capture与总量统计。显示解码器为stdout/stderr分别维护有限UTF-8尾缀，EOF/真正非法编码明确替换，不把两个流的尾缀拼成字符。
也可以在生产者形成合法文本块前处理字节边界，但不得失去channel/backpressure/无换行输出上限。不需要为了修一个字符缓存整行。

### 回归

2/3/4字节字符跨3997–4001附近各边界、EOF、交错stderr和原本非法UTF-8；合法输入不能制造U+FFFD，raw artifact仍逐字节一致，既有预算和取消不退化。
官方函数语义：[Rust String::from_utf8_lossy](https://doc.rust-lang.org/std/string/struct.String.html#method.from_utf8_lossy)。

## KV与主体下一步

[当前回执](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/docs/reviews/2026-09-18-review-c8a62355/KV_SEQUENCE_COMPLETENESS_RECEIPT.md)记载完整B0/B1前缀、交付区间、成本桶补全；本轮读取的[测试后段](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/agent-compose/tests/kv_production_sequence.rs#L1580-L1825)确认真实continuation链及Unknown字段断言。
此测试明确所有调用为Main、maintenance=0、cache read/write/miss全部Unknown。因此可以证明缺测不补零，不能据此宣称已经测到真实有值缓存桶、维护路径和净费用收益。
保留现有测试，补本地受控的有值读/写/失败/重试与真正触发维护的同任务轨迹。真实供应商接受、命中与费用仍分别NOT_RUN，不能因没有付费实验而阻塞本地修正。
官方规则入口：[Prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)。精确前缀和工具契约参与匹配；稳定key不替代相同输入。

## 维护性与未升级观察

H1/H2收敛“相关性/缓存价值”和“终结语义权威”；H3/H5收敛扫描、交付、字符语义；H4收敛失败阶段与恢复许可。
不新建第二目录/第二调度器，不进行全仓按行数拆文件。每片使用真实调用边界回归，定向完成后继续主体。

`ExternalMap`某些访问戳保留旧card claim是当前注释明确允许的折中，本轮没有将它单列为缺陷；不能把所有元数据变化都机械要求重写卡片。
已经存在的新contained_spawn与稳定journal锁应保留；本轮未对它们补做Windows/并发实测。
文档只更新下一动作与回执链接，避免把本报告全文再追加进默认CURRENT/NEXT工作集。
