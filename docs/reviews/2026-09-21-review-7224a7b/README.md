# V5 耐久测试根因审查包（固定提交 7224a7b）

固定分支 `codex/runtime-endurance-full-plan`，固定提交 `7224a7baa8910881b9efaa77f14013781137ff49`。
本包是**固定提交的测试根因审查与相关 Runtime 链路审查**，不是全仓逐行阅读或全工作区测试通过的认证。

- [详细审查](REVIEW.md)：F01–F11 根因、证据分层、覆盖边界、修复顺序与回归规格。
- [实施任务](NEXT_ACTIONS.md)：按报告第 6 节顺序切片的本地实施清单与退出条件。
- [实施回执](BATCH13_RECEIPT.md)：本批实际改动、真实命令与剩余限制。
- [零供应商反例](probes/probe_v5_oracle.py)：oracle 不变量反例脚本。
- [基线结果](probe_results_baseline.json)、[基线标准输出](probe_stdout_baseline.log)：审查时实际执行结果（4 个不变量漏检、退出码 1）。
- [被测 oracle 字节副本](sources/scripts/package_endurance_v5/oracle.py)：Git blob `b459e27152c6b2ea8cb8416ce25cc5002eebe3ab`。
- [源包清单](audit_manifest.json)。

复现（在包含本源码的 checkout 上执行）：

```bash
python -B docs/reviews/2026-09-21-review-7224a7b/probes/probe_v5_oracle.py \
  --repo . --out /tmp/probe_results.json
```

`--repo` 指向仓库根或只含 `scripts/package_endurance_v5/oracle.py` 的目录。**退出码 0 表示四类不变量已全部拒绝**，
退出码 1 表示仍有漏检（基线值）。该脚本只创建临时合成 SQLite 仓库，不执行候选实现、不运行 Rust Runtime、
不调用网络或供应商。

本目录中的 `REVIEW.md`、`probes/`、`sources/`、`probe_results_baseline.json`、`probe_stdout_baseline.log`、
`audit_manifest.json` 为审查包原始字节（仅文件名归入仓库约定）；`NEXT_ACTIONS.md`、`BATCH13_RECEIPT.md`
与 `MANIFEST.json` 为本轮本地实施产物。原始 `NOT_ACCEPTED_MODEL_BUDGET_EXHAUSTED` 回执与冻结证据
（`docs/experiments/package-endurance-v5/`）未被改写。
