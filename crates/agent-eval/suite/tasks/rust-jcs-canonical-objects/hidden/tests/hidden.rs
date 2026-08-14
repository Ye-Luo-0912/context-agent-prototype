use jcs_canonical_objects::{canonicalize, Json};

#[test]
fn objects_are_key_sorted_without_whitespace() {
    let value = Json::Object(vec![
        ("b".into(), Json::Number(1)),
        ("a".into(), Json::Number(2)),
    ]);
    assert_eq!(canonicalize(&value), "{\"a\":2,\"b\":1}");
}

#[test]
fn nested_array_and_literals() {
    let value = Json::Array(vec![
        Json::Null,
        Json::Bool(true),
        Json::String("x\"y".into()),
        Json::Number(-3),
    ]);
    assert_eq!(canonicalize(&value), "[null,true,\"x\\\"y\",-3]");
}
