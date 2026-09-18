# 耐久 campaign 最小证据包（脱敏）

2026-09-19 归档。来源：`target/runtime-endurance-v1/incremental-platform-20260919/`（未入库的本机产物；2026-09-19 审查在其环境只能拿到 404，本包把回执级子集冻结进仓库）。逐文件 sha256 见 [EVIDENCE_MANIFEST.json](EVIDENCE_MANIFEST.json)（含源路径与字节数）；逐项证据映射见 [EVIDENCE_MAP.json](EVIDENCE_MAP.json)。不重写任何既有回执/报告。

## 包含

- `receipts/`：campaign 总回执、final app 验收（PASS）、baseline-lock（head `24c354cb`、fixture/runtime binary 哈希）、L0、soak（108,000 条 / 0 mismatch）、P3 冒烟与崩溃恢复、crash 回执流。
- `controllers/`：五个测试控制器/验证器源码（P3 冒烟、P3 崩溃、soak、receiver 对账、final app 验收）——审查时缺失的"负载控制器源码"在此冻结。
- `segments/`：8 个模型 segment 的 `metadata.json`（head、binary 哈希、保护基线、预算、模型、wire 口径）、`summary.json`（轮次/工具/终结事件）、`usage-ledger.json`（**估计值**逐请求账）、prompt；P3–P8 的纠正指令文本。
- `final-app/`：模型＋人工修复后的最终交付物身份冻结——`app/`、`tests/`、`oracle.py`、`TASK.md`、`fixtures/`（oracle 复算所需的输入）。

## 不包含（及原因）

- 逐请求 wire 捕获（`request-*.json`/`response-*.sse`，MB 级）与 `events.jsonl`、`runtime.patch`：本包定位为回执级最小证据，非完整重放材料。
- SQLite 运行数据（jobs/cas）、进程日志。
- 一切凭据： relay 设计为凭据不落盘（metadata `wire_capture: forwarding relay; credentials omitted`），归档前对全部包含文件做过 `sk-`/`Bearer`/`OPENAI_API_KEY`/`api_key` 模式扫描（唯一命中为 "ta**sk-i**solation" 误报）。

## 分层口径（对应审查 E-1）

| 层 | 状态 | 证据 |
|---|---|---|
| 自主交付 | 不宣称 | `operator_acceptance=false`、`task_state=awaiting_operator_review` |
| 人工修复后验收 | PASS | `final-app-verifier.json`＋`manual_repairs[4]`＋final-app 冻结身份 |
| Runtime 故障覆盖 | 部分 | F12 campaign 内 exercised；F10+F12 组合有确定性回归（`d1b75da5`）；其余 NOT_EXERCISED（见 EVIDENCE_MAP） |
| 应用负载 | PASS（测试控制器＋应用 worker） | soak-receipt：90 分钟/108,000 条/0 mismatch——不是 Rust Runtime 耐久证据 |
| KV 命中 | OBSERVED（T8，Chat 协议） | [walkthrough](../../walkthroughs/2026-09-18-t8-kv-live.md)；跨运行首请求 hit=1536，段总 16896/11 轮 |
| 费用对照 | 仅 runner 估计 | 峰值 $1.0377822 为 relay 自身常量估计；独立复算/配对布局对照/金额正规化 NOT_RUN |

## 已知限制

- 人工修复**前**的应用快照未被 campaign 捕获，只有最终状态与修复清单。
- usage-ledger 全部为 runner 侧估计（`estimated: true`），不是供应商账单。
- baseline-lock 的 head 是 campaign 启动时的 `24c354cb`（分支后续提交不改变该冻结身份）。
