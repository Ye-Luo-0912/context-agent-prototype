# 局部回归与验证矩阵

基线：`12c86283b8d5991e9f17a07f14871dcf39d65066`。日期：2026-09-06。

**下表是建议补入现有测试体系的用例，不是已通过清单。** 本轮没有 Rust 工具链与完整 checkout，未运行这些 Rust/Windows/产品集成测试。唯一实际执行的是文末注明的三个探针程序。这里不创建新的全仓总门禁，不要求无关功能等待全部实验完成。

## A. 持久化与生命周期

| 对应问题 | 建议位置 | 故障/输入 | 必须断言 | 本轮状态 |
|---|---|---|---|---|
| STORAGE-01 | agent-storage 代际恢复测试；Windows 进程测试 | 已完成 g1→g2；在 g2→g3 元数据替换中终止进程 | 缺 metadata 但有 .gN 时拒绝空初始化；journal id 不被静默重建 | NOT_RUN |
| STORAGE-01 | 同上 | 最新 .gN 为部分写入或未发布 | 不能仅按最大文件名代际选取恢复源 | NOT_RUN |

产品分支（2026-09-06）已在 `agent-storage` 补：缺 metadata 且有 WAL 代际（含未发布 `.g3`）时 `RecoveryRequired`、不铸 g1/新 journal id；`replace_file` 覆盖写后目标仍在。Windows 在 metadata 替换中途杀进程仍未跑。
| STORAGE-02 | agent-storage 故障注入 | metadata rename 成功后 dir sync 失败 | 当前 writer 不接受旧代新记录；再次打开选择与对账结果一致 | NOT_RUN |
| STORAGE-02 | agent-storage / agent-core 显式 compact | metadata 发布后 seek 失败；调用者捕获错误继续使用 | 显式 compact 失败形成持久的本进程写入围栏，或经过证明的恢复 | NOT_RUN |
| STORAGE-02 | agent-core 普通追加 | 自动 compact 在 append 中失败 | 保持已有 recovery_required 防护；修复不弱化普通追加 | NOT_RUN |
| PROCESS-01 | tool-runtime proof runner + agent-compose crash fixture | 真实 host recipe 执行中 SIGKILL 宿主，再启动新进程 | 不存在无监督旧子进程继续变更 workspace；监督身份与权限身份分开 | NOT_RUN |
| PROCESS-01 | 同上 | recipe 派生子进程；spawn 到登记间的断点 | 清理直接子进程不被当成整棵树已清理；缺确认应显式失败 | NOT_RUN |
| PROCESS-02 | agent-process supervisor 测试 | 非 PGID 首领的 child，仅调用 reap | 超时后的直接 kill fallback 不因持锁失效；结果准确 | NOT_RUN |
| PROCESS-02 | 同上 | 注入 wait 错误、kill 失败和第二次 wait 超时 | 未确认终态不能报告清理成功/无条件清空监督身份 | NOT_RUN |
| 上轮 EOF 等待问题 | tool-runtime process/shell 测试 | 子进程关 stdout/stderr 后仍 sleep | EOF 后 deadline 与 cancel 仍生效，无忙循环，最终 reap | NOT_RUN |

## B. 文件、协议与边界

| 对应问题 | 建议位置 | 故障/输入 | 必须断言 | 本轮状态 |
|---|---|---|---|---|
| WORKSPACE-01 | agent-workspace confined/runtime_facts | Cargo.toml 是无写端 FIFO | 项目标记探测有界返回；不在同步 open 卡死 | NOT_RUN（OS 机制另已验证） |
| WORKSPACE-01 | 同上 | 普通文件、.git 目录、symlink/reparse | 标记规则不误伤合法目录；正文只能进入支持的文件类型 | NOT_RUN |
| WORKSPACE-02 | Windows confined 测试 | 重复 reparse 拒绝/句柄信息查询失败 | handle 数量稳定，拒绝仍生效 | NOT_RUN |
| PROVIDER-01 | provider-openai loopback HTTP fixtures | 非 2xx 大 body、持续 body、多字节 UTF-8 | 在读入而非显示阶段限额；取消、超时、Retry-After 保持正确 | NOT_RUN |
| PROVIDER-02 | Chat/Responses 成对 fixture | Chat length + DONE，Responses max_output_tokens incomplete | 两条路保留等价的不完整终止信息，不把 length 丢为正常完成 | NOT_RUN |
| PROVIDER-02 | 同上 | 部分文本已被 sink 暴露 | 不透明重放；按既有 exposed-output 规则处理 | NOT_RUN |
| PROVIDER-03 | Responses framing fixture | 相同矛盾 event/type 分别有/无尾部空行 | 正常路径与 EOF flush 校验一致，都拒绝矛盾 | NOT_RUN |
| MCP-01 | MCP duplex/stdio fixtures | 对端停止读，请求超出管道可写空间，再 cancel | 不等待完整 request_timeout 才处理取消；半帧 session 被毒化 | NOT_RUN |
| MCP-01 | 同上 | 写入/flush 发生 I/O 错误 | 返回的清理状态有 await reap 依据，不只发出 kill | NOT_RUN |
| MCP-01 | adapter lazy connect | 等锁或握手期间取消 | 停止启动新请求；既有子进程按明确规则清理 | NOT_RUN |

## C. Context、打包与产品配置

| 对应问题 | 建议位置 | 输入 | 必须断言 | 本轮状态 |
|---|---|---|---|---|
| CONTEXT-01 | context-simple dependency/index tests | 同实体 64、65、128 个 live 条目 | 全部情况下 newest-first 的定义一致，不只在 cap 内成立 | NOT_RUN |
| CONTEXT-01 | 同上 | dead 前缀 + 新 live 条目；update_entities 后桶换位 | dead 不耗尽可用候选配额；顺序不能依赖已被 swap_remove 破坏的存储排列 | NOT_RUN |
| CONTEXT-01 | 同上 | 多实体重叠桶 | 扫描工作量与新增候选量分别有界，去重不改变排序定义 | NOT_RUN |
| 上轮消费 ACK | context-simple 消费测试 | debug 与 release 各运行相同物化/ACK | 状态更新不得只存在于 debug_assert 表达式内 | NOT_RUN |
| 上轮重复选择 | context-simple materializer | 非 pinned PromptRequired + 充足预算 | item 只加入一次、计费一次 | NOT_RUN |
| PACKAGE-01 | 现有打包脚本 smoke | 自定义 target、CARGO_TARGET_DIR、fresh/reused dist | 本次构建产物与复制源同一身份；无旧 helper 混入 | Bash 桩测试已执行；真实构建 NOT_RUN |
| PACKAGE-01 | Windows PowerShell smoke | cargo 非零退出且磁盘有旧 exe | 整个打包非零退出，不生成“成功”的旧包 | NOT_RUN |
| PACKAGE-01 | helper 打包选项 | 全新 checkout 与增量 checkout | helper 是否随包只由显式配置决定，不由旧文件是否存在决定 | NOT_RUN |
| DOC-01 | 文档维护 | CURRENT 已落地/待办列表与实际文件 | 删除矛盾的活动执行指令；不用全量语义黑名单平台替代编辑 | 人工静态核对已完成，仓库文档脚本未本地执行 |

## D. 本轮真正执行的探针

从解压后的审查包根目录运行，需受控 Linux、Python 3.11+（宿主探针使用 process_group）、Bash。程序创建临时文件/子进程；不会连接真实 Provider，不会调用真实 cargo。

```bash
python3 probes/dist_script_probe.py
python3 probes/fifo_open_probe.py
python3 probes/host_crash_probe.py
```

这些程序输出 JSON。需要保存新的重跑结果时，使用新文件名，避免覆盖本轮证据。脚本验证的是已审查基线，包含“当前问题存在”的断言，不是修复后应继续通过的产品验收程序。修复后的回归应改为正确性断言，并在仓库既有测试中实现。

- `dist_script_probe.json`：两个 stale 包场景退出 0；Bash 构建失败对照退出 17。
- `fifo_open_probe.json`：同类 flags 阻塞；NONBLOCK + fstat 识别 FIFO；子进程已回收。
- `host_crash_probe.json`：父进程 SIGKILL 后子进程存活；随后 kill group + waitpid；两者均已回收。

**这三项不能替代 Rust 单元/集成测试、Windows 测试或真实 Agent crash-resume。**
