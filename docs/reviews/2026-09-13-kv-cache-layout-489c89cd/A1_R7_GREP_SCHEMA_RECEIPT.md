# A1/R7 实施回执：search.grep 的 scan_continuation 进入模型可见 schema

日期：2026-09-13。来源：[KV 缓存布局续审](REPORT.md) R7（先修）。基线：F1–F5 合并后的 main（`489c89cd`＋本批 docs 调和）。

## 用户结果

模型现在能从正式 ToolSpec 发现并生成 `scan_continuation` 续跑参数：声明经 `compact_for_model_surface` 后仍保留，按 schema 形状构造的参数经公开 execute 路径（真实 dispatch 入口）到达第二批扫描——不再只是「解析器认识但模型永远看不到」。

## 缺陷（R7）

`GrepArgs` 与执行分支已支持 `scan_continuation`（sealed handle，与结果分页 cursor 分离），coverage footer 亦指导续跑——但 `SearchGrepTool::spec` 的 `input_schema.properties` 只有 pattern/path/limit。严格参数生成/校验的 provider 可能放大此缺口。

## 实现

`SearchGrepTool::spec` 的 input_schema 新增 `scan_continuation` 属性：`type: string`、`maxLength: 256`（与真实 sealed locator ≈125 字符对照验证过——回执附实测断言 `handle.len() <= max_len`）、description 写明同查询语义（handle 绑定原 query，换 pattern/加宽 path 会被拒绝——与 `load_scan_state` 的既有校验一致）与「原样回传、不得编造」。`cursor` 仍不在 schema（parser-only 结果分页兼容），两 handle 不混用。

## 回归（红检查在先）

`spec_schema_declares_the_scan_continuation_and_schema_shaped_args_dispatch`（search.rs 测试模块）：

1. **红**：schema 未声明时断言 `properties.scan_continuation` 存在 → 实测失败（"scan_continuation must be declared in the model-visible input_schema"）。
2. 声明存在且 `maxLength` 有界（100..=512 内，实测覆盖真实 handle 长度）→ 绿。
3. `compact_for_model_surface()` 后声明仍在（compactor 只截 description/剥 schema description，不删 properties——实测断言钉住）→ 绿。
4. **dispatcher 端到端**：batch 1 强制 PARTIAL（6 文件 limit 3）→ 取 handle → batch 2 参数**严格由 schema 声明的属性构造**（仅 pattern＋scan_continuation，limit/path 缺省）→ 经公开 execute 到第二批 → 命中未扫描文件（file_03/04/05）→ `hits_total=6`、`scan_complete=true` → 绿。

## 验证（实际执行）

- `cargo test -p tool-runtime spec_schema_declares`：红（修复前）→ 绿（修复后）。
- `cargo test -p tool-runtime`：**271 通过**（既有 270＋新增 1），0 失败；既有 cursor/continuation 分离测试（`a_result_page_cursor_and_a_scan_continuation_stay_distinct` 等）全部保持。
- `cargo clippy -p tool-runtime --all-targets`：0 警告；`cargo fmt -p tool-runtime` 干净。

## 未验收（如实记录）

- 未实测特定 provider 对新增可选参数的接受行为（审计 R7 同样未实测，表述为「不宣称所有模型必然拒绝」）——真实 provider 归条件窗口。
- `cursor` 维持 parser-only；模型可见分页仍走 artifact.read。
