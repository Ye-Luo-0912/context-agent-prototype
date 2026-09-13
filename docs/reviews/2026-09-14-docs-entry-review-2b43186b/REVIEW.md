# 续审：文档入口应与后端主体一起收口

文档主审查快照：`2b43186b005b5e86172037f98206f4417c7e2bca`；提交时间 2026-09-13 19:18:13 UTC（东京时间 2026-09-14 04:18:13）。该快照 CI run 34777279921 已成功，绑定该 SHA；收尾已有后续提交，见下。

## 收尾更新：新提交不应继续按未提交处理

审查期间，main 更新为 `278280146df5934828962253507553762693f494`（UTC 2026-09-13 21:19:53 / 东京时间 2026-09-14 06:19:53）。提交包含 A 线 process/session/MCP 修复以及 C 线路由键、组合根与 HTTP 捕获回归相关改动。前面的 `2b43186b` 只提交回执而未提交 A/C 代码的事实已成为历史；本包 CURRENT/NEXT 草案已更新，不能据旧记录重复实现。

新 SHA 的 CI run `34783505990` 首次运行为 failure。作业摘要显示两个平台 Clippy 失败、Rust 测试矩阵跳过；文档检查与 .NET 作业通过。读取 Linux 作业 `103794681532` 日志，定位到 `crates/tool-runtime/src/tools/session.rs:2156` 的 `assert_eq!(...is_null(), false)`，Clippy 要求 `assert!(!...is_null())`。这是小型集成修复，不是重新开启整个构建阶段。未在本地修复或执行任何测试。

新 CURRENT 顶部追加了“A 线已代码落地”，但后面仍把 `6eda2474` 称为当前 main 并保留原 N01–N10 待办叙述，正是本轮建议“替换当前工作集而不是继续追加”的具体例子。新 `engine.rs` 的 blob SHA 仍是 `ffa9afd476efa34a367348b6536d38e508b12fd6`；本轮发现的 hydration 完整性传播残余仍适用于该已核对文件。

收尾覆盖：提交元数据与部分 diff、CURRENT 顶部、ModelInput 部分片段、hydration 同一 blob、CI 作业及 Linux 日志；没有完整通读新提交所有变更，不把本次 delta 检查称为全仓审查完成。

来源：[新提交](https://github.com/Ye-Luo-0912/context-agent-prototype/commit/278280146df5934828962253507553762693f494)、[新 CI](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34783505990)、[Linux 失败作业](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34783505990/job/103794681532)、[新 CURRENT](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/278280146df5934828962253507553762693f494/docs/CURRENT.md)。

## 结论

需要优化，而且首先是开发指令与状态来源的正确性，而非润色。根 AGENTS 仍派桌面主线，CURRENT/NEXT 以追加历史为主，state.json/STATUS/README 保留另一套旧当前状态。继续追加本次报告不会解决此问题。

本包提供 5 份入口替换草案（含上述收尾状态更新）、README 局部替换和一次性迁移任务，未远端修改仓库。生产代码续审只确认新提交相关调用链，不宣称全仓逐行完成。

## D1：默认指令与用户当前方向相反

根 AGENTS 当前目标仍是 M17 多入口工作台，三线含 C 桌面产品，且说基础修复不整体阻塞 GUI。主审查基线的 NEXT 则明确 GUI 不进主线，A/B/C 已是另一组归属。修订时把阶段从 AGENTS 移出，仅保留稳定开发约定和阅读入口。

依据：[AGENTS](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/AGENTS.md)、[NEXT](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/docs/NEXT_TASKS.md)。

## D2：活动文档变成了历史正文全集

GitHub tree 元数据给出的 UTF-8 文件大小：CURRENT 122,317 bytes，NEXT 189,638 bytes，两份合计 311,955 bytes（约 304.6 KiB）；ARCHITECTURE 151,116 bytes，AUDIT_TODO 84,092 bytes。这里不是 tokenizer 统计，也不声称这些字节一定全部进入每次请求。

CURRENT 顶部仍把 6eda2474 说成当前 main、把 N01/N02 列成待修；NEXT 已记载 B1–B3 合入。较低段落又继续保留 R7 缺失、M18 未提交/全绿等旧叙述。完整历史应保留，但不应要求每个新执行者先消解这种时间关系。

做法：CURRENT 仅当前事实，NEXT 仅仍有动作的任务，已闭项只留简短成果及来源链接。之前审查报告中的历史、反例和任务脚本也不再整份复制到当前页面。

依据：[CURRENT](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/docs/CURRENT.md)、[NEXT](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/docs/NEXT_TASKS.md)、[目录元数据](https://api.github.com/repos/Ye-Luo-0912/context-agent-prototype/git/trees/de8818c108cf75b5182b138dd6fa6cd2aa731e04)。

## D3：至少三处旧状态不应再用于派工

README 和 STATUS 仍称 N0 当前、旧 fmt CI 失败。state.json 的 updated 为 2026-09-07，active_task 指向 GUI N4，default_context 为 dynamic-in-process；正式 host 未指定策略却使用 Rolling。当前 HEAD 的 CI 已成功，不能继续引用旧失败充当当前阻塞。

状态应绑定 source_sha + environment + verification_scope，不要求每次文档提交去更新成自己的 SHA；避免自引用更新循环。README 不维护当前 CI，STATUS 只导航，state.json 明确历史角色或退役，默认配置以实际入口为准。

依据：[README](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/README.md)、[STATUS](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/docs/STATUS.md)、[state.json](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/docs/state.json)、[host/main.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/crates/agent-host/src/main.rs)、[本次 CI](https://github.com/Ye-Luo-0912/context-agent-prototype/actions/runs/34777279921)。

## D4：文档检查通过不等于路线/实现一致

现脚本只检查 state 必需键、历史报告路径、少数旧短语、13 个指定文件的链接路径以及 Rust toolchain。没有核对当前路线、默认值、代码生产接线或未提交实现；输出的“links and state agree”比实际验证范围更宽。NEXT 的 C1/C2 长行还混入竖线和旧问题全文，表格本身已经不适合承载当前任务卡。

不应继续累加禁止短语。先把文件职责定清，再保持小而机械的结构、路径和引用检查；源码是否实现仍由对应代码验收证明。

依据：[doc_consistency.py](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/scripts/doc_consistency.py)、[NEXT](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/docs/NEXT_TASKS.md)。

## D5：本地回执与 main 状态应分开

最新提交明确说 A/C 回执已经入库、代码仍未提交。NEXT 本身也有该限制，不能反过来指控其谎报 main 已落地；问题是限制埋在很长表格里，且同页存在多代“已收口”。应把下一动作改为“核对现有 diff、集成、验证”，而非再次开发 N04–N10。

最少记录代码位置、实现状态、生产接线证据、验证范围。LOCAL_REPORTED/LOCAL_VERIFIED 不自动变成 MERGED/CI_VERIFIED。CI 只覆盖指定 SHA，真实供应商测试独立列出，NOT_RUN 不必阻塞普通接线。

依据：[最新提交](https://github.com/Ye-Luo-0912/context-agent-prototype/commit/2b43186b005b5e86172037f98206f4417c7e2bca)、[B 回执](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/docs/reviews/2026-09-14-backend-review-6eda2474/B_LINE_RECEIPT.md)、[NEXT](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/docs/NEXT_TASKS.md)。

## 代码续审：B2 修复后的完整性没有传到调用方

已确认底层改变：hydration 先复制读取计划，不再先移除 owner；暂时 I/O 失败保留 pending；卡片读取与结构校验已加强。旧 N01–N03 不原样重开。

新的控制流接缝：hydrate_all_pending_cards 遇“没有安装、没有消费”的批次就返回，返回类型仍是 ()。storage_gc_protecting 随后继续调用 plan_storage_gc，原 roots_complete 不变。plan_storage_gc 只遍历已安装 external 的强引用；pending source 的引用边未知时，保护闭包可能缺失。reconcile/search 等调用方同样应检查未完成状态。

有条件的反例：待载入 A 强引用符合清理条件的 B；A 读卡暂时失败；B 没有其他独立保护根。此时 pending A 虽未丢失，但 A→B 的保护边仍可能漏出删除规划。控制流已确认，实际 Rust 故障注入反例未执行；不能称为已经观察到用户数据丢失。

最小处置：先在同一 op_gate 中把 pending 未排空判为元数据/引用完整性不足，延期不可逆清理；或让 hydration 返回可用的完整性结果。搜索诚实报告不完整，reconcile 不把 pending owner 当孤儿重新认领。不要仅加入 pending 的 id 后声称引用闭包已完整；未读出的出边仍然未知。

依据：[engine.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/crates/context-simple/src/engine.rs)（hydrate_all_pending_cards、storage_gc_protecting、reconcile_store_protecting）；[store.rs](https://github.com/Ye-Luo-0912/context-agent-prototype/blob/2b43186b005b5e86172037f98206f4417c7e2bca/crates/context-simple/src/store.rs)（plan_storage_gc）。

## 文档与 KV 缓存的关系

开发本仓库的 coding agent 所读取的说明，与本项目 Runtime 发出的模型请求，是两条不同链路。缩短入口可减少前者的无关阅读；只有文档实际进入后者的请求，才可能影响其 KV 匹配和费用。磁盘上保留大量归档本身不产生 token 费用。

稳定约定保持小而稳定；当前任务状态单独组织，按需读取契约与历史证据。应先排除旧/冲突指令，再讨论缓存。正文缩短、前缀复用率提高、缓存写入减少和任务总费用下降不是同一个指标。

官方参考：[Codex AGENTS](https://developers.openai.com/codex/guides/agents-md/)（指令发现范围与字节限制，不把所有被链接文档都误算为自动加载）；[OpenAI Prompt Caching](https://developers.openai.com/api/docs/guides/prompt-caching/)（按当前端点文档核对支持范围，不推广到所有兼容网关）。

## 本轮覆盖与未执行

完整返回并阅读：AGENTS、STATUS、doc_consistency.py、B_LINE_RECEIPT。

局部返回并阅读：README 1–105；CURRENT 顶部请求 1–32（响应截断，非全文）；NEXT 顶部请求 1–220（响应截断，非全文）；ROADMAP 顶部请求 1–180（响应截断，非全文）；state.json 1–140；ARCHITECTURE 1–85；engine.rs 870–1285、1880–2075、2740–2865；store.rs 1190–1375；host/main.rs 100–175；actor/model.rs 65–145。树元数据用于大小/路径统计，不计入全文阅读。

运行环境再次尝试 git clone，DNS 无法解析 github.com；command -v 未找到 Cargo/.NET。没有执行仓库测试、文档 gate、真实模型或费用实验。没有访问用户本地 A/C 工作树。没有修改/推送仓库或覆写历史报告。

本包本地仅验证：草案文件实际存在、UTF-8 可读、文件清单/字节数记录、ZIP 完整性。它不是仓库验收证据。
