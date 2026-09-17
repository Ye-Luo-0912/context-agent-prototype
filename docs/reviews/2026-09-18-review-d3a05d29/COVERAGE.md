# 实际读取范围 — d3a05d29

固定源码：`d3a05d297295da1ece66b00245fa027f2ee12852`。本轮实际读取 **20 个不同文件**的全文或指定区间。

**未完成全仓所有源码、测试和脚本逐行覆盖。** 目录、文件名、提交 diff、CI 中的测试名称不计作源文件通读。未读取部分不能据此判定没有缺陷。

| 文件 | 实际范围 | 原工具来源 |
|---|---|---|
| [Cargo.toml](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/Cargo.toml) | 全文 | turn636file0 |
| [crates/tool-runtime/src/tools/page.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/page.rs) | 全文 | turn637file0 |
| [crates/tool-runtime/src/tools/fs.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/fs.rs) | 1–550 返回被截断；430–735 完整区间。不能将前一次请求范围全部视为已读。 | turn638file0, turn639file0 |
| [crates/tool-runtime/src/tools/artifact.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/artifact.rs) | 1–280；280–645 | turn641file0, turn642file0 |
| [crates/agent-process/src/contained_spawn.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/agent-process/src/contained_spawn.rs) | 1–295 | turn643file0 |
| [crates/context-simple/src/engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/engine.rs) | 1–195；530–760；1570–1830；2530–2680；2700–2940；3270–3455 | turn664file0, turn644file0, turn645file0, turn647file0, turn646file0, turn670file0 |
| [crates/context-simple/src/gc/reachability.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/reachability.rs) | 1–340；340–620；780–1000（另复核1–235） | turn650file0, turn659file0, turn661file0, turn678file0 |
| [crates/context-simple/src/index/external.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/index/external.rs) | 1–315；315–640 | turn651file0, turn655file0 |
| [crates/context-simple/src/gc/full/mod.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/full/mod.rs) | 1–310 | turn656file0 |
| [crates/agent-storage/src/lib.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/agent-storage/src/lib.rs) | 55–280；575–790；940–1110 | turn653file0, turn657file0, turn658file0 |
| [crates/context-simple/src/gc/minor.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/gc/minor.rs) | 1–315 | turn660file0 |
| [clients/dotnet/Agent.Client/DeltaCoalescer.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/clients/dotnet/Agent.Client/DeltaCoalescer.cs) | 全文 | turn662file0 |
| [clients/dotnet/Agent.Client/BoundedEventQueue.cs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/clients/dotnet/Agent.Client/BoundedEventQueue.cs) | 全文 | turn663file0 |
| [crates/context-simple/src/index/entity.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/context-simple/src/index/entity.rs) | 1–300 | turn665file0 |
| [crates/agent-contracts/src/operation.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/agent-contracts/src/operation.rs) | 1–210 | turn666file0 |
| [crates/tool-runtime/src/tools/process.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/process.rs) | 1–220 | turn667file0 |
| [crates/tool-runtime/src/tools/stream.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/tool-runtime/src/tools/stream.rs) | 1–230；220–420（返回已到文件结尾） | turn668file0, turn669file0 |
| [docs/reviews/2026-09-18-review-c8a62355/KV_SEQUENCE_COMPLETENESS_RECEIPT.md](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/docs/reviews/2026-09-18-review-c8a62355/KV_SEQUENCE_COMPLETENESS_RECEIPT.md) | 全文；仅作为作者回执，不能冒充本环境实测 | turn671file0 |
| [crates/agent-compose/tests/kv_production_sequence.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/agent-compose/tests/kv_production_sequence.rs) | 1580–1825；其余部分未在本轮通读 | turn672file0 |
| [crates/agent-host/tests/host_t7_journey.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/d3a05d297295da1ece66b00245fa027f2ee12852/crates/agent-host/tests/host_t7_journey.rs) | 1250–1405 | turn677file0 |

## 验证层级

本环境未安装 Cargo/Rustc/.NET，Git 访问 DNS 失败，未取得源码 checkout。源代码来自固定 SHA 的 GitHub 连接器。
`mechanism_probes.py` 只实现局部机制检查；H2、H4 是静态调用链发现，没有冒充已运行的 Rust 复现。
CI 是远端结果，不是本环境运行；回执文件中的作者测试结果单独归属作者。

## 不应推导的结论

没有报告全仓已无其他问题；没有从小窗口推断完整源文件无匹配；没有以作者回执替代所有代码检查。
TUI 本轮的直接新发现来自共享工具输出通道；没有重新逐行通读 TUI 八个源文件。
本轮未修改、推送或创建仓库 PR，也没有使用真实供应商凭据。
