# 续审覆盖与阅读深度

本轮代码基线见 SOURCE_MANIFEST.json；全量词法扫描见 STRUCTURAL_SCAN.json。418 个源码/测试/构建文件、285,276 行，20 个 Rust crate 以及 SDK/桌面/发布入口。与上一轮相比 70 个文件内容不同、1 个新增；没有假设同 HEAD 就是同实现。

## 阅读深度

| 范围 | 本轮阅读方式 | 本轮产出 |
|---|---|---|
| context-simple GC full/minor/reachability/residency | plan、root、sweep、IO、commit、终态、TTL、Pending、测试链逐段读 | R2-03/04/05/06 |
| scope / scope_tree / indexes / heap / catalog | 结构与生命周期、候选生成、scope close、序列化、移出路径检索 | R2-05/07，性能建议 |
| store / materializer / directive / access / checkpoint | required、读写身份、四种 owner、存储删除保护、restore、召回路径 | R2-04/05/06/07 |
| distill / compactor / rolling | 当前修复全文关键流程、失败/裁剪/重试/费用分支与测试入口 | R2-11；COST-8；摘要验收残余 |
| Runtime Actor / task / checkpoint / safepoint / restore | 材料化新 lane、控制循环、GC/保存等待、完整验证、热裁剪与历史查询 | R2-01/02/08/09/12 |
| contracts model/context/event | 根强度、窗口与 usage 语义、新字段、类型化降级、恢复限制 | A/B/C 接口要求 |
| provider-openai | 当前 Chat/Responses usage、独立维护 transport、retry 与 diagnostics/缓存既有实现复查 | R2-10/11，COST-6/7/8 |
| Core / Storage / process / capability-process | 当前 operation/审批/监督/WAL/资源规则的接口和关键边界复读，其余结构扫描 | 保留既有权威与清理规则；未新增该层漏洞结论 |
| tool-runtime | 搜索/读取/修改/验证与 Core 的调用边界沿前轮检查，当前结构扫描与变更比对 | 保留前序修复，不因这次 GC 审查重写工具层 |
| host / compose / TUI / replay | 当前默认 profile、启动 restore、maintenance 接线、公开 task 读入口与有界载入 | R2-09/12，实际产品配置限制 |
| platform protocol / .NET SDK / GUI | 公共状态/事件字段搜索、task/成本/恢复消费面与前轮记录比对；其余结构扫描 | 新接口单一合入与消费验收要求；未逐控件审核 |
| context-contextcore / context-service | 替换引擎契约、序列化/超时/材料化边界沿前轮追踪，当前结构比对 | 不把 Simple 的局部取消成功外推到所有替换引擎 |
| agent-eval / agent-conformance | 当前费用聚合关键分支与既有依赖矩阵/相关测试，其他 harness 结构扫描 | 账目完整性问题；复用原 COST-5 |
| CI / scripts / manifests | 当前源清单和结构比对，沿用前轮已读入口；未运行构建发布 | 不外推 CI/发布通过 |

`READ_RANGES.jsonl` 记录辅助阅读器请求的范围及当时文件摘要，不是“每行都已在工具输出显示”的证明。早期 rg/Get-Content 阅读未全部记入该日志，部分宽输出有截断；报告问题的判定分支后来做了窄范围复读。

## 不在逐行语义阅读覆盖中的内容

大部分测试的全部实现、冻结实验/seed/golden/suite 仓库、生成代码/obj/bin，以及本轮只做结构扫描的外围实现没有逐行读完。JSON fixture 数据不在该源码行数口径；只核对与接口变化相关的 fixture/测试入口。没有将“扫描了所有文件”写成“所有路径运行验证通过”。

如果后续工作树漂移，必须重新读被修改的相关模块。当前列出的缺陷都有独立源码链路，不靠未读文件的猜测成立；不把全量词法扫描命中转成新任务。
