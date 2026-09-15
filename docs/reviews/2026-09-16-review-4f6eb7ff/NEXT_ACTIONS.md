# 当前阶段可执行补充（4f6eb7ff）

沿现有 A/B/C 队列合并，不再新增宏观阶段。详细触发与证据见 REVIEW.md。

## B：冷页仍是逻辑 owner（先做 V1，连同 V2）

用户动作：历史已经外置/分页后，Agent 仍可正确要求这份证据进入下一次请求；运行 GC 不会让未读卡片失去有效 scope。

修改入口：context-simple 的 gc/full、scope retirement、materialize/required 解析及目录所有权 API。

必须完成：把元数据/引用完整性传到 scope 退休许可；为精确 required 引用提供有界冷元数据解析；存在但未加载不报 Missing。不要移除坏卡片/坏 scope 校验，也不要用全历史加载替代分页。

停止条件：同一正文在已加载与分页状态下的语义保证一致；目标反例通过，正常退休仍可收敛。

## B/C：一次搜索结果和它的 coverage 是同一个对象（V3–V5）

用户动作：Agent 得到部分命中时知道尚有未读区间，并能在固定预算内正确继续；恢复或目录变更后旧游标不能错用。

修改入口：ContextEngine 结果契约、SimpleContextEngine continuation、service op/handler/adapter、Core model_content 投影。

必须完成：fresh 与 resume 分开；去重且有界的 walk 状态；成功 restore 后失效；目录代际绑定；service 转发查询/续查并返回对应 coverage。未知旧服务不默认 complete。不要让多个额外 getter 承担一次调用应原子返回的事实。

停止条件：固定历史上重复普通搜索不增大 retained state；固定预算续查推进；in-process/service 同一 fixture 结果语义相同；restore/旧 token 反例通过。

## C：缓存块映射符合实际端点（V6）

用户动作：明确标记可复用工具结果时，请求使用合法的输入内容块，不发错误类型或未经确认的 fallback。

修改入口：Responses mapper、既有 endpoint_shape_tests 与 cache wire 集成。

必须完成：类型/位置/消息展开后的映射一起验证；工具结果输入使用受支持输入块；不能承载的缓存提示显式拒绝/剔除并解释。保持工具 call/result 配对和真实正文。普通文本已修部分不重做。

停止条件：独立 fixture 覆盖块类型而非仅字段存在；本地生产链路一致。真实命中与成本继续由条件实验验证，不报未测降本。

## A/C：取消不抹掉已知费用（V7）

用户动作：失败调用已报告费用，随后用户取消重试，账目仍保留已知数值，执行状态仍是取消。

修改入口：retry settlement、生产 compose、Runtime 模型 operation outcome/accounting。

必须完成：outcome 与 usage 正交；不设置 OPENAI_RETRY_METRICS_FILE 也有必有的正式结算；诊断 observer 是副本。保留取消屏障和代际隔离，不把旧操作输出重新发布。

停止条件：带 usage 的失败→backoff cancel 的正式链路回归通过；未知不补零、未执行尝试不多计。

## 集成约束

共享契约由单一集成人维护；行为修改和机械大范围移动分开；定向测试后跑既有相关跨 crate 集成，合并时跑 CI。CURRENT 只改当前事实，NEXT_TASKS 只保留尚需动作；本补充不要整份追加到所有入口。

GUI 仍维护模式。不要引入第二个 Runtime、第二套任务数据库或通用新评估框架。所有新增回归在本报告生成环境中均未执行。
