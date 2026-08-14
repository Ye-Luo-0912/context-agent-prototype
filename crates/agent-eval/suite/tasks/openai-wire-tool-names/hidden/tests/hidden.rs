use openai_wire_names::{mappings, to_wire_tool_name};

#[test]
fn dotted_and_colon_names_become_underscores() {
    assert_eq!(to_wire_tool_name("fs.list"), "fs_list");
    assert_eq!(to_wire_tool_name("mcp:tool"), "mcp_tool");
    assert_eq!(to_wire_tool_name("get_time"), "get_time");
}

#[test]
fn mappings_restore_core_ids() {
    let map = mappings(&["fs.list", "get_time"]).expect("no collision");
    assert_eq!(map.get("fs_list").map(String::as_str), Some("fs.list"));
    assert_eq!(map.get("get_time").map(String::as_str), Some("get_time"));
}

#[test]
fn colliding_core_ids_fail_closed() {
    let err = mappings(&["fs.list", "fs_list"]).expect_err("collision");
    assert!(err.contains("fs.list"), "{err}");
    assert!(err.contains("fs_list"), "{err}");
}
