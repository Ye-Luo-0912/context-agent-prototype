# DeepSeek Flash 真实 provider 端到端回执（2026-09-12，用户授权执行）

授权：用户明确提供 DeepSeek 密钥并要求写入环境文件与执行端到端测试（解除了 TASKS.md「本轮不调用付费模型」对本轮的限制；仅限本回执记录的有界范围）。密钥存放于 `eval.env`（已确认 gitignored），仓库内零泄漏（全仓扫描排除 eval.env 零命中），报告/文档只含 MODEL/BASE_URL。

## 配置（eval.env）

- `OPENAI_BASE_URL=https://api.deepseek.com/v1`、`OPENAI_MODEL=deepseek-flash`、`OPENAI_API_PROTOCOL=chat`（COST-2 新落的 DeepSeek 顶层 hit/miss 映射即 chat SSE 协议）、`OPENAI_MAX_OUTPUT_TOKENS=4096`。
- Host 输出确认生效：`provider profile: deepseek-flash @ https://api.deepseek.com/v1 protocol=chat context_window=128000 max_output_tokens=4096 sampling=provider-default prompt_cache=provider_default`。

## 端到端 ①：真实产品走查（live_walk，真实 compose 栈）

`f6_three_live_product_walkthroughs`（真实模型＋真实工作区工具＋真实文件变更验证，预算 FULL_ROUNDS 有界）：**通过，39.45 秒**。

1. **修真实 bug**：读 calc.py 发现 add 返回 a−b → edit.patch 修复 → process.run 跑 Python 验证（add(2,3)=5、add(-1,1)=0、mul 保持）全部通过。`calc.py` 变更 ✓。
2. **加小特性**：`app.py`/`config.py` 变更 ✓，turn-completed。
3. **跨文件重构＋中断/继续**：`files.py`/`names.py`/`users.py` 变更 ✓；行为保持由进程验证（normalize 语义全部保持）。**OperatorClosureOnly 语义正确执行**：task.complete 被 completion gate 以类型化 `operator_required` 拒绝——模型完成工作与验证但不自行持久关闭任务。
- 走查记录（含模型轮次、工具清单、验证输出）写入 `docs/walkthroughs/2026-09-06-f6.md`（该文件为走查记录的既有落点，本run覆盖更新；密钥扫描零命中）。

## 端到端 ②：正式 host 二进制＋生产 SDK（新增回归，密钥存在才运行）

`HostChainTests.Real_provider_host_end_to_end_with_deepseek_flash`（无 AGENT_DEMO；OPENAI_* 从 eval.env 注入 host 进程环境）：**通过**。

全链：SDK → Named Pipe → 正式 agent-host → 真实 provider 轮次 → 工具落盘 → TurnCompleted，随后断言 `hello.txt` 内容逐字节精确；同 run 上 PLATFORM-1 精确收据查询双臂——已受理 id 读回 `Accepted`（绑定 task）、未见 id 读回 `Unknown`（IsIndeterminate，绝无否定断言）。

**过程中发现并修正的测试侧缺陷**：初版等待任务到达 `Completed`——违背 OperatorClosureOnly（模型不可自行持久关闭任务，live_walk 记录亦证实 gate 拒绝）。修正为以**工件本身**（文件存在＋内容精确）为完成信号，并补上**操作员审批循环**：正式 host 运行交互式审批门，模型的工具调用需操作员 Allow 才执行——这正是产品的知情审批语义，此前两轮 4 分钟超时正是审批未被响应所致（审批放行后 4 秒完成）。

## 调试与诊断清理

临时插桩（safepoint/turn/tools 的 DBG-ALLOC/DBG-CALL/DBG-DURABLE）全部移除；fmt/clippy 干净；dotnet 全套 **118/118**、runtime lib **401**、actor **86**、turn **133**、workspace **111**、context-simple **333**（A 线 CTX-2/3 落定后全绿）。

## 未跑与限制（如实记录）

- `kv_cache_walk` 5 个 ignored 探针**未跑**：其为 2026-09-10 缓存验收的复现 harness，钉死 `responses` 协议＋无 `/v1` base_url＋`KV_CACHE_LIVE=1`，且文档规则明确「不重复相同条件的付费调用」；本端到端走 chat 协议（COST-2 新映射），缓存读/写的线上验证归 COST-2/COST-5 的后续有界执行。
- turn safepoint 1 个失败（非 provider 相关）：见 [EXEC4 验收回执](EXEC4_BOUNDED_LOADS_VERIFICATION.md) 集成注记的精确归因（EXEC-1 重构时序影响＋测试钉死捕获次数；已给出断言放宽处置，归 turn 测试域后续落地）。
- 未提交、未推送、未跑远端 CI。真实成本金额未核（无计费凭据），仅 usage 数字可用。
