# V4：真实模型的多租户快照长流程

2026-09-20，用户要求开始高难度、长流程、复杂真实测试，暴露问题并优化。
沿用当前 DeepSeek Flash 凭据，独立创建 campaign，不续写或重置 v2/v3 的账本。
工作树 HEAD 为 `71322074596b7e604be5c8750d350e7d626b0e28`，保留现有未提交修改。

## 任务与独立判定

真实模型在已验收的 assisted 部署应用上新增 `app/snapshot.py`：多租户历史与
current authority 的确定性 ZIP 导出、严格只读检查、原子恢复、重复导入、真实
发布前进程崩溃恢复。需求详见 [SPEC](../../../scripts/package_endurance_v4/SPEC.md)。
模型只能修改候选模块与其自测，基础应用、规范、fixtures 和公开 smoke tests 受保护。
独立 oracle 在控制器侧构造真实 SQLite/文件和坏 ZIP，不能用候选 helpers 定义正确答案。

关键验收包含：历史/当前身份与连续 generation；确定性字节；URI 特殊字符；
缺失/损坏/跨租户；路径穿越、重复、多余成员、链接和资源上限；拒绝不改输入；
非匹配目标不覆盖；真实 exit 73 后重试；只读前后完整字节核对。

## 硬边界

- 新 campaign 总时限 4 小时，正文清单含不可重置的创建/截止时间。
- 最多 96 次 Runtime 主决策、128 次供应商 attempt、384 次工具尝试。
- 累计输入上限 4000000 token、输出上限 250000 token；每请求输出上限 8192。
- 估算费用上限 USD 1，使用官方高峰价作保守估算：输入 miss 0.3、hit 0.006、
  输出 1.2 USD / 1M token。请求先预留，缺测/未知用量保留负债并停止新受理。
- Chat 协议显式配置非思考模式；维护模型调用上限 0。凭据仅在进程内使用，
  不打印、不进入请求/响应捕获或归档。估算不等于实际供应商账单。
- 所有网络仅为所配置的真实供应商；候选应用不能调用网络或运行数据 payload。

## 连续执行与停止条件

1. 先本地验证控制器的 Chat 协议/usage 和显式非思考配置，不能由 relay 暗改语义。
2. 真实模型在 16–24 决策的小段内开发，保留每段请求、用量、TaskId、输入 lineage、
   checkpoint/restore、工具事实与退出/清理状态。
3. 段间冷恢复同一任务，传入独立验收失败事实及长纠正；每次仅使用原剩余额度。
4. 取消仅在可观察到真实工具/进程在途且供应商 attempt 已结算时执行；若现有入口
   无法证明安全条件，诚实记录未触发，不拿杀模型请求替代取消完成。
5. 应用通过独立验收后，再以多进程反复导出/恢复/只读核对和真实 crash 重试跑
   有界持续负载；只验证已通过验收且摘要固定的候选版本。
6. 安全边界、预算、用量未知或清理未确认会停止对应分支并保留证据。暴露的产品
   或控制器缺陷先复现再修，定向验证后才恢复；不清账、不改原失败标签。

真实模型产物、测试控制器/Runtime 修复、必要的 assisted 应用补修分别记账。
模型自称完成和通过少量 smoke 都不是最终验收。不提交、推送或运行 release。

官方协议依据：[Chat Completions](https://api-docs.deepseek.com/api/create-chat-completion/)、
[Thinking Mode](https://api-docs.deepseek.com/guides/thinking_mode/)、
[价格表](https://api-docs.deepseek.com/quick_start/pricing/)。
