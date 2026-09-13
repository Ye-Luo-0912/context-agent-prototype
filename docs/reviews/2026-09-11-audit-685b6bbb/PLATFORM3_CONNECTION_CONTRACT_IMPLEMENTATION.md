# PLATFORM-3（B2）实施回执：正式连接契约——工作区身份、run epoch 与端点解析

基线 `685b6bbb`（阶段基线，PLATFORM-1 同树续接）。本切片在**工作树**落地（未提交、未推送、未跑远端 CI）。对应 [NEXT_STAGE_THREE_TRACKS.md](NEXT_STAGE_THREE_TRACKS.md) PLATFORM-3、[REPORT.md](REPORT.md) F05 的残余半（默认入口的 GUI 侧修复已由 GUI 线 C1 完成，本切片补平台侧）。

## 用户能获得什么

- 客户端从相对路径、符号链接或不同拼法指向**同一个工作区**时，推导出与宿主 bind 相同的默认端点——不再因「按什么字形哈希」而连不上或连错。
- 每份快照回答「**哪个 run、哪个工作区**产生了这份事实」：重连（尤其共享默认端点上换了宿主化身、或宿主恢复了同一个 run）之后，客户端能分辨回答者身份，而不是把旧 run 的事实当新事实用。
- 既有单用户隔离、grant 撤销、有界帧、背压与 resync 全部保持；版本/schema 漂移在每帧被拒绝（见「已满足、不重做」）。

## 实现落点

**WorkspaceIdentity 解析规则对称化（缺陷修复，核心）**

- 现场缺陷：端点区分符的哈希规则两侧不一致——Rust 宿主 `agent-host::workspace_endpoint_suffix` **先 canonicalize 再哈希**（`lib.rs` wrapper，规范化失败回退原样），而 .NET `AgentTransports.WorkspaceEndpointSuffix` **按原样字节哈希**（旧注释还声称「两侧都不规范化」）。相对路径、含 `sub/..` 的冗余组件、符号链接的 CWD 在两侧推导出不同端点：相同工作区解析不一致，直接违反验收标准。
- 规则定义（两侧注释一致）：**区分符 = 对「规范化后的工作区绝对路径」做字节哈希；规范化由各方用自己平台语义完成；宿主的 bind 是权威；共享 fixture 只钉哈希原语本身（字节→16 hex），钉不了各 OS 的规范化语义。**
- Rust：新增回归 `workspace_endpoint_suffix_resolves_redundant_path_components`（经 `sub/..` 指向同一目录的两种拼法必须哈希出同一 suffix；宿主实现本就正确，此为钉住规则的回归）。
- .NET（`clients/dotnet/Agent.Client/Transports.cs`）：
  - 新增 `WorkspaceIdentity.Resolve(root)`：`Path.GetFullPath` → 去尾部分隔符（**盘根 `C:\` 永不折叠成盘相对的 `C:`**）→ 存在时经 `ResolveLinkTarget(returnFinalTarget: true)` 跟随最终链接目标；每一步失败保持原形，不做超出平台可证明的猜测。输出 `Root`＋`EndpointSuffix`＋`Display`（"workspace <root>"）。
  - `AgentTransports.DefaultSocketPathFor`／`DefaultLocal()`／`DefaultEndpoint()` 全部改经 `Resolve`（旧实现按原样 CWD 字节哈希）。
  - 原语 `WorkspaceEndpointSuffix(string)` 保持逐字节不变——`endpoint_derivation.json` 跨语言金样继续直接钉住它。
- 剩余语义缺口由快照闭合：客户端侧解析做不到与宿主逐位一致（中间组件符号链接等平台差异），因此** equality 的权威是快照里的宿主 canonical root**——客户端解析结果与之不符即说明端点指向别的工作区。

**快照公开 run epoch 与工作区身份（`agent-platform-protocol`、`agent-runtime/src/platform/work.rs`）**

- `WorkSnapshotResponse` 新增 `run_id: RunId` 与 `workspace_root: String`（校验：run_id 非 nil canonical UUID；workspace_root 走 `validate_opaque`，上限 `MAX_SNAPSHOT_WORKSPACE_ROOT_BYTES = 4096`，拒绝空串与控制字符）。deny-unknown wire 不变。
- 宿主路由填充：`run_id` 取自 `RuntimeStatusSnapshot`（本就存在，此前未投影）；`workspace_root` 取 `Workspace::root()` 的 canonical 形式（失败回退原样；Windows verbatim 前缀 `\\?\`／`\\?\UNC\` 剥离为展示形——那是 canonicalize 的表示残留，不是工作区名的一部分）。
- 共享金样 `snapshot_response.json` 增加两字段，Rust／.NET 双侧 roundtrip 继续逐字节一致。

**已满足、不重做（如实记录判断）**

- 「版本不兼容明确拒绝」：信封 `protocol` 块每帧经 `ValidateMatches`（.NET）／`NegotiatedContractProfile`（Rust）校验，name/major/minor/schema digest/features 任一漂移即拒。协商 profile 的事实由每帧信封承载、客户端 `NegotiatedIdentity` 可展示——**不再向 payload 复制同一事实**（避免第二真相源）。
- Windows 默认共享管道名（`focus-agent.platform.v1`）保持不变：DACL＋逐连接令牌校验＋`FILE_FLAG_FIRST_PIPE_INSTANCE` 拒绝接管是既有安全设计；「不同工作区不误连」在本切片由快照的 `workspace_root` 比对闭合（共享端点上换了宿主时客户端可机检），Windows 多工作区默认端点作用域化留给真实多工作区需求出现时再议，不抢做。

**.NET DTO 与测试双体（`clients/dotnet`）**

- `WorkSnapshotResponse` 增加 `RunId`／`WorkspaceRoot` 属性（声明顺序对齐金样键序，roundtrip 逐字节）＋镜像校验。`AgentConnection.SendAsync` 的接收路径 payload 校验（F19）使**缺身份字段的快照应答被诚实拒绝**——六个测试文件的手写快照载荷随之补齐身份字段（`EventStreamTests`、`ClientSafetyTests`、`FramingAndConnectionTests`、`ResilienceTests`、`RestoreWalkthroughTests`、`WorkbenchIntegrationTests`），这是契约同步机制按设计起效。

## 回归（新增 4 项＋既有用例扩展）

- 协议 `snapshot_identity_facts_are_mandatory_and_bounded`：nil run id／空 root／控制字符 root／超限 root 全部拒绝，恰在 4096 边界合法。
- 宿主 `workspace_endpoint_suffix_resolves_redundant_path_components`（见上）。
- host e2e（named pipe＋UDS 双跑）：快照断言 `run_id == handle.run_id()`、`workspace_root == workdir 的 canonical 展示形`。
- .NET `Snapshot_identity_facts_are_mandatory_and_bounded`（镜像协议校验规则）、`Workspace_identity_resolves_before_hashing`（相对与绝对拼法解析后同 suffix；原语保持拼写敏感（钉住规则分层）；盘根不折叠；空白拒绝；Display 含 root）。
- 既有扩展：`Snapshot_fixture_is_bounded_typed_and_watermarked` 增加身份断言；快照金样双侧 roundtrip。

## 实际执行的检查（本机 Windows，2026-09-11，共享树并行多线在飞）

按时间序、在稳定窗口捕获：

- `cargo test -p agent-platform-protocol --lib`：**47 通过**（含本切片新增 1 项；期间并行 PLATFORM-2 的 artifact 分页 2 项先后落地入计数）。
- `cargo test -p agent-platform-protocol --test work_fixtures`：**10 通过**。
- `cargo test -p agent-host --lib`：**8 通过**（含新增 suffix 回归）。
- `cargo test -p agent-host --test host_e2e`：**8/8**（一轮单败为文档已记载的负载抖动类，单独与整包重跑均绿）。
- `cargo test -p agent-runtime --test actor`：**80 通过**。
- `cargo fmt -p agent-platform-protocol -p agent-runtime -p agent-host -- --check`：本切片文件通过（其后并行线在 `agent-runtime/tests/turn/effects.rs` 的新增暂未格式化，不属本切片）。
- `dotnet test clients/dotnet/Agent.Client.Tests`：**108/108**（基线 101＋本切片净增，含更新后的测试双体）。
- `dotnet build clients/dotnet/Agent.Client`：0 警告 0 错误。

**捕获窗口之后的共享树状态（如实记录）：** 验证期间并行核心线（RuntimeEvent usage 字段）与并行平台线（PLATFORM-2 artifact 分页）先后在同一批共享文件上落码；最终窗口的全树 `cargo check`／clippy 受其编辑中态（`agent-contracts` 语法错误→恢复→`work.rs` 半成品→落定）影响无法归属于本切片；.NET 侧此后出现的唯一失败（`Artifact_response_validates_truncation_truth_and_base64`，`work.artifact.next_offset`）属 PLATFORM-2 自身中间态，非本切片文件。

## 未验收 / 边界（如实记录）

- 未提交、未推送、未跑远端 CI；「关闭」待 CI run 记录确认。
- 客户端侧规范化是 best-effort 镜像（叶链接解析），中间组件链接等平台差异不做逐位对齐——相等性判断的权威是快照 `workspace_root`，此为本切片的设计决策而非未竟项。
- Windows 默认管道保持共享名＋快照比对闭合误连（见上），未做每工作区管道名。
- GUI 对 `RunId`／`WorkspaceRoot`／`WorkspaceIdentity.Display` 的面板呈现归 C 线（C3/C4），本切片只交付 SDK 事实。
- 真实 Linux UDS 的生产入口端到端照旧由 CI Linux job 承接（本机 Windows）；真实 provider 照旧 NOT_RUN。
