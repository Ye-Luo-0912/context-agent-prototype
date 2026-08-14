//! Provider 出网时的函数名编解码。自检用已知正确补丁，不展示给模型。

use std::collections::HashMap;

/// `.` 与 `:` 换成 `_`，得到 OpenAI 可接受的函数名。
pub fn to_wire_tool_name(name: &str) -> String {
    name.replace(['.', ':'], "_")
}

/// 线名 → 原始 Core id。两个不同 Core id 落到同一线名时 fail-closed。
pub fn mappings(names: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut from_wire = HashMap::new();
    for original in names {
        insert_mapping(&mut from_wire, original)?;
    }
    Ok(from_wire)
}

fn insert_mapping(
    from_wire: &mut HashMap<String, String>,
    original: &str,
) -> Result<(), String> {
    let wire = to_wire_tool_name(original);
    if let Some(existing) = from_wire.get(&wire) {
        if existing != original {
            return Err(format!(
                "tool names '{existing}' and '{original}' both serialize to '{wire}'"
            ));
        }
        return Ok(());
    }
    from_wire.insert(wire, original.to_string());
    Ok(())
}
