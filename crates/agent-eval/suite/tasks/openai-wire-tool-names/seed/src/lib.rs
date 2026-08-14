//! Provider 出网时的函数名编解码。当前实现是有意写坏的收割题种子。

use std::collections::HashMap;

/// 应当变成 OpenAI 可接受的函数名；种子原样返回，所以带 `.` / `:` 的 id 非法。
pub fn to_wire_tool_name(name: &str) -> String {
    name.to_string()
}

/// 线名 → 原始 Core id。种子在碰撞时 last-wins，不会报错。
pub fn mappings(names: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut from_wire = HashMap::new();
    for original in names {
        from_wire.insert(to_wire_tool_name(original), (*original).to_string());
    }
    Ok(from_wire)
}
