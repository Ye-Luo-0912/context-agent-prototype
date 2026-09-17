# KV 序列完整性回执 — 完整稳定前缀、真实交付证据、完整成本口径

提交：`1680e181`（只改 `crates/agent-compose/tests/kv_production_sequence.rs`，+878/−40）。任务规格：[NEXT_ACTIONS.md](NEXT_ACTIONS.md) C（续）；分析：[REVIEW.md](REVIEW.md) 第 8 节；既有基础：[KV_SEQUENCE_RECEIPT](../2026-09-16-review-71f8a586/KV_SEQUENCE_RECEIPT.md)（九批）。

## 保留（不重做）

九批全部断言原样保留：真实磁盘逐字节断言、同任务 key＝`routing.key_for(task,"main")`、B0 钉位、工具加载/撤销、checkpoint/真实重组/restore、失败结算、账本精确各一次。真实端点接受/命中/净费用仍 NOT_RUN 归 T8。

## 补全的三类验证

**1. 完整稳定前缀**：每轮**整个** `input[0..=B0]` 对第 1 轮基线逐项比较（九批只比 `input[b0]` 一项）；相邻轮整段**声明的公共断点前缀**轮内必须逐字节一致（证据区不得轮内漂移）；轮首分歧只允许发生在 B1 证据项且保持 `SELECTED WORKING CONTEXT` 结构；所有失败输出给出首个分歧项下标＋两侧预览。参与匹配的 tools/schema 在相邻轮同工具集时按整块 canonical 比较。19→26 轮全绿。

**2. 真实交付证据**（依赖 G2/G3，随其落地转绿）：轨迹扩为 26 轮/8 回合，新增 T6b 走读段——预置 `logs/big.log`（600×150 字符，页中唯一块 ID L100/L300/L500＋首尾对照 L10/L590，种子时磁盘断言）。走读**由捕获服务器从工具刚返回的 continuation 子句逐字生成**下一读请求（"原样使用工具给出的 continuation"），实际观测 9 页（1-98、99-196、197-200、201-297、298-394、395-400、401-497、498-594、595-600；工具先走完请求窗口再给续子句，故请求窗口有意重叠而**交付声明链**必须连续）：每个后续页必须逐字等于先前返回过的子句且先于产生它的请求；交付声明 1→600 无缺口无重叠、条数＝请求页数；每页恰好一个交付 body、携带 `lines=S-E/600`、**无 runtime 截断标记**（任何剪裁即 DELIVERY REGRESSION）；每个块 ID 在声明覆盖它的页上按**真实源行号**交付、不泄漏到其他页。脚手架文案（EXPECTED-RED）已移除——现在红即真回归。

**3. 完整成本口径**：`LedgerRow` 扩展 cache read/write/miss 三桶（`CostCounter::{Known,Unknown}`）、event 展开缓存、attempts、retries、`role: Main/Maintenance`、typed usage 是否上报；期望行改为从服务器**实际服务**的决策派生（`served`）；固定前段逐字先行、Fail 殿后、生成段全是 fs.read＋收尾完成的断言；输入 token 身份唯一（同一累计 snapshot 不会重复计）。全 Unknown 缓存桶显式断言（绝不补零）；总额＝逐行之和＝脚本总额。诚实边界写进注释/断言：脚本值只证明传输与结算，不证明真实命中率/价格收益。

## 红绿证据

- fs.read 扩展合树前：EXPECTED-RED 签名（三页页中块 ID 全缺＋`runtime_truncated_marker=true`）双跑一致；G2/G3 落地后转绿。
- 三类断言的红-first 演示（本文件内临时种子、逐个回退）：预 B0 分歧→`round 8: the FULL stable prefix input[0..=B0] diverges ... at item 0`；轮内证据漂移→`rounds 17->18: ... diverged at item Some(2) (mid-turn...)`；`Known(0)` 缓存桶→扩展行等值失败（九批等值从不检查该字段）。
- **最终绿**：`cargo test -p agent-compose --test kv_production_sequence` 四连绿 **14.00–14.06s**（rustfmt 前后各两跑）；全套 compose 其余目标全绿。

## 已执行验证（Windows，cargo 1.97.1）

- `cargo test -p agent-compose`（全套，含 proof_supervision、KV）：全绿。
- `cargo check -p agent-compose --tests`：全部测试目标编译通过。

## 边界（如实）

- 走读首请求（1-200）仍是脚本化的模型自由首读；第 2 页起纯 continuation 跟随。
- `LedgerRow` 的 attempts=1 断言来自事件 `attempts` 字段＋每行一个捕获 body 佐证；typed `ModelUsage.attempts`（0=未知）按原语义记录不重释。
- 供应商缓存匹配仍要求按其规则分别观察写入与读取；相同路由 key 不代替相同前缀——真实端点三态保持 NOT_RUN。
