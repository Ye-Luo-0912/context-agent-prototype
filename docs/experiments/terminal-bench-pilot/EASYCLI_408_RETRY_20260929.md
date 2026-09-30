# EasyCLI 上游 HTTP 408 有界重试（2026-09-29）

## 触发与范围

TB43 Rolling 的第 16 次 Responses 流在约 600 秒后由 EasyCLI 返回 HTTP 408，且未到 `response.completed`。Runtime 退出时保留了一笔用量 unknown。TB43 原回执继续作为该窗口的固定证据；本次实现处理后续窗口，不改写或重放那笔请求。

本地 EasyCLI profile 现在只对“上游在响应头发送前返回 HTTP 408”准许一次重试。relay 把这一状态映射成 Runtime 现有重试逻辑可识别的 HTTP 503，并记录 `retryable_upstream_408`。只有请求体 SHA-256 与原请求一致时才放行；其他请求仍收到 423。重试再次遇到 408、失败或产生其他用量未知时，relay 冻结后续请求。已开始发送的 HTTP 200 流发生断连时不会由 relay 重放。

第一次 408 的 input/output 保留预约不释放，精确重试也新增一笔预约；成功结算只提交重试响应的 usage，旧请求仍标记 unknown。后续请求继续受累计 token、请求数、峰值费用和 Core grant 上限约束。该处理能防止预算账本把未知请求当成零成本，但不能判断 EasyCLI 是否已为第一次请求计费，因此一次重试可能发生重复扣费。

EasyCLI profile 的 Runtime 请求超时为 660 秒，host relay 上游等待为 600 秒；MiMo 与 Pinaic 设置不变。请求超时仍有界。这只让上游错误更可能先到达可分类边界，不会修复上游在 600 秒时断开的问题。

## 已执行验证

- `python -m unittest scripts.tests.test_terminal_bench_pilot scripts.tests.test_terminal_bench_relay`：36 项通过测试，1 项平台条件跳过。合成 relay 用例覆盖一次精确重试后继续、不同请求体被拒、重试后未知预约保留，以及再次收到 408 后冻结；均未访问真实供应商。
- `python -m py_compile scripts/terminal_bench_pilot/credential_relay.py scripts/terminal_bench_pilot/run_bounded_b_pair.py scripts/terminal_bench_pilot/harbor_agent.py scripts/tests/test_terminal_bench_relay.py scripts/tests/test_terminal_bench_pilot.py`：通过。
- `cargo test -p agent-compose main_request_timeout_has_a_finite_validated_override --lib`：1/1 通过。Cargo 输出 Windows 增量目录清理的 Access Denied 警告；构建和该测试成功。
- WSL 执行 `cargo build --locked --target x86_64-unknown-linux-gnu -p agent-tui --bin agent-tui`：成功，耗时 1 分 35 秒。新 ELF SHA-256：`54d07c9ae673619399a1e32178d6aaff45e0f96849752fcb9ba1b5b0cc5d3144`。
- TB44 `--preflight-only`：`STATIC_READY_NO_PROVIDER_CALLS`。固定题包、镜像、重建 ELF、价格快照和候选 grant 身份均已核验；预检没有访问模型或修改题包。

候选 grant 文件为 `scripts/terminal_bench_pilot/grants_tb44_retry_candidate.json`，10 条规则的路径、风险、次数及内容上限与 TB43 候选相同，仅到期时间改为 2026-09-29 13:00（Asia/Shanghai），SHA-256 `45e12784db437e5bef5581136a545ef410411528e228089d93877e3b7ea70680`。用户批准后实际用于 TB45；TB44 首次 agent setup 因 glibc 基线不符而停止，relay attempts=0、provider calls=0。改用 Debian 12 构建后 TB45 以新 identity 运行；见 [TB44/TB45 真实回执](TB44_TB45_EASYCLI_RETRY_20260929.md)。

## 结论限制

TB45 的真实 EasyCLI 配对可比完成，但 Dynamic 与 Rolling 都未遇到 HTTP 408：112/112 次请求均 completed，retry count=0、usage unknown=0，因此真实重试分支仍未被供应商触发。本地 relay 合成回归验证了该分支；TB43 的 unknown 请求没有重放。TB45 reward 仍均为 0，Rolling CTRF 15/18、任务未完成；此前跨 grant 多文件反例是否实际出现未由本回执确认。未来若再开展真实窗口，必须使用当时有效的 Core grant；不得自动延长授权或把本地 direct-API 价格等价值写作 Plus 实际账单。
