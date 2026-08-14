//! 种子：带空白、对象键保持插入顺序。JCS 要求无空白且按 UTF-16 排序键。

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(i64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

pub fn canonicalize(value: &Json) -> String {
    match value {
        Json::Null => "null".into(),
        Json::Bool(true) => "true".into(),
        Json::Bool(false) => "false".into(),
        Json::Number(n) => n.to_string(),
        Json::String(text) => format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"")),
        Json::Array(items) => {
            let inner: Vec<String> = items.iter().map(canonicalize).collect();
            format!("[ {} ]", inner.join(", "))
        }
        Json::Object(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(key, nested)| format!("\"{}\": {}", key, canonicalize(nested)))
                .collect();
            format!("{{ {} }}", inner.join(", "))
        }
    }
}
