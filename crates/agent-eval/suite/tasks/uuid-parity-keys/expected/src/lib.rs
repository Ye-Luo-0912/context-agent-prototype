//! UUID 键折叠。自检用已知正确补丁：按值排序后再分配占位符。

use std::collections::BTreeMap;

pub fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes[8] == b'-'
        && bytes[13] == b'-'
        && bytes[18] == b'-'
        && bytes[23] == b'-'
        && bytes.iter().enumerate().all(|(i, b)| {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                true
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// 收集 UUID 键的值，排序后重键为 `<uuid>`、`<uuid>1>`…，与输入顺序无关。
pub fn collapse_uuid_keys(entries: Vec<(String, i32)>) -> BTreeMap<String, i32> {
    let mut out = BTreeMap::new();
    let mut uuid_values = Vec::new();
    for (key, value) in entries {
        if is_uuid(&key) {
            uuid_values.push(value);
        } else {
            out.insert(key, value);
        }
    }
    uuid_values.sort_unstable();
    for (index, value) in uuid_values.into_iter().enumerate() {
        let placeholder = if index == 0 {
            "<uuid>".to_string()
        } else {
            format!("<uuid>{index}>")
        };
        out.insert(placeholder, value);
    }
    out
}
