//! UUID 键折叠。种子 last-wins，输入顺序会改变幸存值。

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

/// 把 UUID 形键折叠成占位符。种子对 `<uuid>` last-wins。
pub fn collapse_uuid_keys(entries: Vec<(String, i32)>) -> BTreeMap<String, i32> {
    let mut out = BTreeMap::new();
    for (key, value) in entries {
        if is_uuid(&key) {
            out.insert("<uuid>".into(), value);
        } else {
            out.insert(key, value);
        }
    }
    out
}
