# 最终优化验收证据

本目录只记录 2026-09-20 最终本地验证。原 `../evidence/` 和
`../FOLLOWUP_VALIDATION.json` 保持原样；后续修复的源码身份单列，不能把任一旧
PASS 当作不同源码版本的证明。

- `SUMMARY.json`：最终状态、边界、测试与负载数字。
- `TEST_RESULTS.json`：实际命令、平台、成功/忽略/跳过口径与日志链接。
- `SOURCE_IDENTITY.json`：本次修改的关键源码身份；`frozen-inputs.json` 为正式
  负载前冻结的 13 项输入，含 helper、应用控制器与 Windows host/TUI 二进制。
- `application-acceptance.json`、`host-preflight.json`、`host-under-load.json`、
  `host-cleanup/`：当前应用独立验收、独立 host 预检和同库负载中正式旅程。
- `load-30min.json`、`load-timeline.jsonl.gz`、`final-repository-verification.json`：
  完整负载、逐批事实、停止 writer 后的独立 SQLite/文件审计。
- `logs/`：完整执行日志的 gzip 副本，保留首次失败及后续精确复验。
- `host-incomplete-observation.json`、`host-sandbox-access-failure.json`、
  `interrupted-load*.json`、`snapshot-retry-*`：早期失败、中止和修复反例，均不改判。

`verify_final_repository.py` 是实际验证脚本的原样副本。其执行位置为仓库内
`target/optimization-final-validation-20260920/verify_final_repository.py`，脚本的
`__file__` 相对路径按该位置解析；复跑须放回这一位置并提供原新 campaign。
归档副本用于审查执行逻辑，不应直接在本目录运行后误读路径。

所有供应商调用均为回环合成测试，新增付费调用 0。原始完整测试和 campaign
目录仍在 `target/`；本目录是选择的验收证据集，不是全部请求/事件的备份。
`MANIFEST.json` 记录本目录其他文件的 SHA-256，清单不包含自身。
