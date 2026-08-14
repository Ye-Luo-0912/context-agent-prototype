use uuid_parity_keys::collapse_uuid_keys;

fn u(n: u8) -> String {
    format!("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeee{n:02x}")
}

#[test]
fn equal_multisets_match_regardless_of_order() {
    let a = vec![(u(1), 3), (u(2), 1), ("keep".into(), 9)];
    let b = vec![(u(2), 1), (u(1), 3), ("keep".into(), 9)];
    assert_eq!(collapse_uuid_keys(a.clone()), collapse_uuid_keys(b));
    let collapsed = collapse_uuid_keys(a);
    assert_eq!(collapsed.get("<uuid>"), Some(&1));
    assert_eq!(collapsed.get("<uuid>1>"), Some(&3));
    assert_eq!(collapsed.get("keep"), Some(&9));
}
