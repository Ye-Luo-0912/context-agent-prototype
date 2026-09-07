# 审查与下一阶段任务包

从 [REPORT.md](REPORT.md) 看代码问题，从 [NEXT_STAGE_TASKS.md](NEXT_STAGE_TASKS.md) 开始实施。

这是固定提交 b299c6a 的定点续审和阶段提案，不是完整源码ZIP，不是全仓审查通过证明，也没有实现或推送新功能。

选型建议：保留Rust执行系统，采用.NET10＋Avalonia正式原生客户端，以受限本地IPC连接；平台、基础、GUI按功能依赖并行。

实际覆盖和未跑项见 [AUDIT_STATUS.json](AUDIT_STATUS.json)、[READ_COVERAGE.csv](READ_COVERAGE.csv)；问题见 [FINDINGS.json](FINDINGS.json)，源码和外部资料见 [SOURCES.md](SOURCES.md)。

`probe_group_lifetime.py` 是已经执行过的隔离Linux机制探针；其结果不等于仓库集成测试。`build_report.py` 只是生成本包的脚本，不是仓库修改脚本。
