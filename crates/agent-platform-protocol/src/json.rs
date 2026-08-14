//! 解析期 JSON 预算：编码帧上限不能单独挡住 DOM 放大。
//!
//! `[{},{},…]` 一类载荷在帧字节内合法，解码成 `serde_json::Value` 时每个空
//! 对象仍会分配 Map。这里在 **Visitor 前进时** 计量深度、节点、字符串与
//! 容器宽度，一旦越界立刻失败，而不是先建成整棵树再校验。
//!
//! 这不是 RFC 8785/JCS，也不把现有适配器迁到 Platform 信封；typed DTO 的
//! 字段预算仍走各自的 `validate()`。

use std::fmt;

use agent_contracts::AgentError;
use serde::de::{
    self, DeserializeOwned, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor,
};
use serde_json::{Map, Number, Value};

/// 控制面默认深度。query/cancel 信封浅，远低于 serde_json 的 128。
pub const MAX_JSON_CONTROL_DEPTH: usize = 16;
/// 控制面单字符串（含对象键）字节上限。
pub const MAX_JSON_CONTROL_STRING_BYTES: usize = 4_096;
/// 控制面全部字符串累计上限，对齐 16 KiB 信封。
pub const MAX_JSON_CONTROL_TOTAL_STRING_BYTES: usize = 16 * 1024;
/// 控制面单数组长度。
pub const MAX_JSON_CONTROL_ARRAY_LEN: usize = 64;
/// 控制面单对象键数。
pub const MAX_JSON_CONTROL_OBJECT_KEYS: usize = 64;
/// 控制面节点总数（每个 JSON 值算一个节点，键不算）。
pub const MAX_JSON_CONTROL_NODES: usize = 512;

/// 数据面最大嵌套深度。比 serde_json 默认递归上限更紧。
pub const MAX_JSON_DATA_DEPTH: usize = 32;
/// 数据面节点上限：与编码字节解耦，避免空对象数组按帧长放大。
pub const MAX_JSON_DATA_NODES: usize = 65_536;
/// 数据面单数组长度上限。
pub const MAX_JSON_DATA_ARRAY_LEN: usize = 65_536;
/// 数据面单对象键数上限。
pub const MAX_JSON_DATA_OBJECT_KEYS: usize = 4_096;

/// 一份解码预算。所有上限至少为 1，否则连 `null` 也会被拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonDecodeBudget {
    pub max_depth: usize,
    pub max_string_bytes: usize,
    pub max_total_string_bytes: usize,
    pub max_array_len: usize,
    pub max_object_keys: usize,
    pub max_nodes: usize,
}

impl JsonDecodeBudget {
    /// 操作控制面：小信封、紧节点。不跟 16 KiB 帧长按比例放大。
    pub const fn control_plane() -> Self {
        Self {
            max_depth: MAX_JSON_CONTROL_DEPTH,
            max_string_bytes: MAX_JSON_CONTROL_STRING_BYTES,
            max_total_string_bytes: MAX_JSON_CONTROL_TOTAL_STRING_BYTES,
            max_array_len: MAX_JSON_CONTROL_ARRAY_LEN,
            max_object_keys: MAX_JSON_CONTROL_OBJECT_KEYS,
            max_nodes: MAX_JSON_CONTROL_NODES,
        }
    }

    /// 与已落地帧上限配套的数据面预算。
    ///
    /// 单字符串/累计字符串不超过帧长（解码串不可能大于已读入的编码字节）；
    /// 节点数按 `frame/8` 估算后封顶，避免 `1 MiB` 的 `{}` 数组变成数十万 Map。
    pub fn for_frame_bytes(max_frame_bytes: usize) -> Self {
        let frame = max_frame_bytes.max(1);
        Self {
            max_depth: MAX_JSON_DATA_DEPTH,
            max_string_bytes: frame,
            max_total_string_bytes: frame,
            max_array_len: (frame / 2).clamp(8, MAX_JSON_DATA_ARRAY_LEN),
            max_object_keys: (frame / 8).clamp(8, MAX_JSON_DATA_OBJECT_KEYS),
            max_nodes: (frame / 8).clamp(32, MAX_JSON_DATA_NODES),
        }
    }
}

/// 解析期失败。预算类错误在 Visitor 内置位，语法错误来自 serde_json。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonDecodeError {
    InvalidUtf8,
    Syntax(String),
    Type(String),
    Depth { depth: usize, max: usize },
    StringBytes { bytes: usize, max: usize },
    TotalStringBytes { bytes: usize, max: usize },
    ArrayLen { len: usize, max: usize },
    ObjectKeys { keys: usize, max: usize },
    Nodes { nodes: usize, max: usize },
}

impl JsonDecodeError {
    fn syntax(error: serde_json::Error) -> Self {
        Self::Syntax(error.to_string())
    }

    fn type_mismatch(error: serde_json::Error) -> Self {
        Self::Type(error.to_string())
    }
}

impl fmt::Display for JsonDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => formatter.write_str("JSON is not UTF-8"),
            Self::Syntax(message) => write!(formatter, "{message}"),
            Self::Type(message) => write!(formatter, "{message}"),
            Self::Depth { depth, max } => write!(
                formatter,
                "json decode budget exceeded: depth {depth} > {max}"
            ),
            Self::StringBytes { bytes, max } => write!(
                formatter,
                "json decode budget exceeded: string is {bytes} bytes, above the {max} byte bound"
            ),
            Self::TotalStringBytes { bytes, max } => write!(
                formatter,
                "json decode budget exceeded: total string bytes {bytes} > {max}"
            ),
            Self::ArrayLen { len, max } => write!(
                formatter,
                "json decode budget exceeded: array length {len} > {max}"
            ),
            Self::ObjectKeys { keys, max } => write!(
                formatter,
                "json decode budget exceeded: object keys {keys} > {max}"
            ),
            Self::Nodes { nodes, max } => write!(
                formatter,
                "json decode budget exceeded: nodes {nodes} > {max}"
            ),
        }
    }
}

impl std::error::Error for JsonDecodeError {}

impl From<JsonDecodeError> for AgentError {
    fn from(error: JsonDecodeError) -> Self {
        AgentError::InvalidRequest(error.to_string())
    }
}

/// 把已读入的一帧解码成 `Value`，并在 Visitor 前进时执行预算。
pub fn decode_value(bytes: &[u8], budget: &JsonDecodeBudget) -> Result<Value, JsonDecodeError> {
    if std::str::from_utf8(bytes).is_err() {
        return Err(JsonDecodeError::InvalidUtf8);
    }
    let mut counters = DecodeCounters::new(*budget);
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = match (BoundedValueSeed {
        counters: &mut counters,
    })
    .deserialize(&mut deserializer)
    {
        Ok(value) => value,
        Err(error) => {
            if let Some(fail) = counters.fail {
                return Err(fail);
            }
            return Err(JsonDecodeError::syntax(error));
        }
    };
    deserializer.end().map_err(JsonDecodeError::syntax)?;
    Ok(value)
}

/// 先按预算建成有界 `Value`，再投影到类型 `T`。
///
/// DOM 炸弹在第一阶段失败；字段形状错误是第二阶段的类型失败。
pub fn from_slice_bounded<T: DeserializeOwned>(
    bytes: &[u8],
    budget: &JsonDecodeBudget,
) -> Result<T, JsonDecodeError> {
    let value = decode_value(bytes, budget)?;
    serde_json::from_value(value).map_err(JsonDecodeError::type_mismatch)
}

struct DecodeCounters {
    budget: JsonDecodeBudget,
    depth: usize,
    nodes: usize,
    string_bytes: usize,
    fail: Option<JsonDecodeError>,
}

impl DecodeCounters {
    fn new(budget: JsonDecodeBudget) -> Self {
        Self {
            budget,
            depth: 0,
            nodes: 0,
            string_bytes: 0,
            fail: None,
        }
    }

    fn fail<E: de::Error>(&mut self, error: JsonDecodeError) -> Result<(), E> {
        self.fail = Some(error);
        Err(E::custom("json decode budget exceeded"))
    }

    fn consume_node<E: de::Error>(&mut self) -> Result<(), E> {
        let next = self.nodes.saturating_add(1);
        if next > self.budget.max_nodes {
            return self.fail(JsonDecodeError::Nodes {
                nodes: next,
                max: self.budget.max_nodes,
            });
        }
        self.nodes = next;
        Ok(())
    }

    fn consume_string<E: de::Error>(&mut self, value: &str) -> Result<(), E> {
        let bytes = value.len();
        if bytes > self.budget.max_string_bytes {
            return self.fail(JsonDecodeError::StringBytes {
                bytes,
                max: self.budget.max_string_bytes,
            });
        }
        let total = self.string_bytes.saturating_add(bytes);
        if total > self.budget.max_total_string_bytes {
            return self.fail(JsonDecodeError::TotalStringBytes {
                bytes: total,
                max: self.budget.max_total_string_bytes,
            });
        }
        self.string_bytes = total;
        Ok(())
    }

    fn enter_depth<E: de::Error>(&mut self) -> Result<(), E> {
        let next = self.depth.saturating_add(1);
        if next > self.budget.max_depth {
            return self.fail(JsonDecodeError::Depth {
                depth: next,
                max: self.budget.max_depth,
            });
        }
        self.depth = next;
        Ok(())
    }

    fn exit_depth(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }
}

struct BoundedValueSeed<'a> {
    counters: &'a mut DecodeCounters,
}

impl<'de> DeserializeSeed<'de> for BoundedValueSeed<'_> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(BoundedValueVisitor {
            counters: self.counters,
        })
    }
}

struct BoundedValueVisitor<'a> {
    counters: &'a mut DecodeCounters,
}

impl BoundedValueVisitor<'_> {
    fn scalar<E: de::Error>(self, value: Value) -> Result<Value, E> {
        self.counters.consume_node()?;
        Ok(value)
    }
}

impl<'de> Visitor<'de> for BoundedValueVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        self.scalar(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        self.scalar(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        self.scalar(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        let number =
            Number::from_f64(value).ok_or_else(|| E::custom("NaN or Infinity is not JSON"))?;
        self.scalar(Value::Number(number))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        self.counters.consume_string(value)?;
        self.scalar(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        self.counters.consume_string(&value)?;
        self.scalar(Value::String(value))
    }

    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        self.scalar(Value::Null)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        self.scalar(Value::Null)
    }

    fn visit_seq<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        self.counters.enter_depth()?;
        self.counters.consume_node()?;
        let mut items = Vec::new();
        while let Some(item) = access.next_element_seed(BoundedValueSeed {
            counters: self.counters,
        })? {
            items.push(item);
            if items.len() > self.counters.budget.max_array_len {
                self.counters.fail(JsonDecodeError::ArrayLen {
                    len: items.len(),
                    max: self.counters.budget.max_array_len,
                })?;
            }
        }
        self.counters.exit_depth();
        Ok(Value::Array(items))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        self.counters.enter_depth()?;
        self.counters.consume_node()?;
        let mut map = Map::new();
        let mut keys = 0usize;
        while let Some(key) = access.next_key_seed(BoundedStringSeed {
            counters: self.counters,
        })? {
            keys += 1;
            if keys > self.counters.budget.max_object_keys {
                self.counters.fail(JsonDecodeError::ObjectKeys {
                    keys,
                    max: self.counters.budget.max_object_keys,
                })?;
            }
            let value = access.next_value_seed(BoundedValueSeed {
                counters: self.counters,
            })?;
            map.insert(key, value);
        }
        self.counters.exit_depth();
        Ok(Value::Object(map))
    }
}

/// 对象键走同一套字符串预算，避免超长键在 `next_key::<String>()` 时绕过上限。
struct BoundedStringSeed<'a> {
    counters: &'a mut DecodeCounters,
}

impl<'de> DeserializeSeed<'de> for BoundedStringSeed<'_> {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(BoundedStringVisitor {
            counters: self.counters,
        })
    }
}

struct BoundedStringVisitor<'a> {
    counters: &'a mut DecodeCounters,
}

impl Visitor<'_> for BoundedStringVisitor<'_> {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object key")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<String, E> {
        self.counters.consume_string(value)?;
        Ok(value.to_owned())
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<String, E> {
        self.counters.consume_string(&value)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn empty_object_array(count: usize) -> Vec<u8> {
        let mut bytes = Vec::from(&b"["[..]);
        for index in 0..count {
            if index > 0 {
                bytes.push(b',');
            }
            bytes.extend_from_slice(b"{}");
        }
        bytes.push(b']');
        bytes
    }

    fn nested_arrays(depth: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.resize(depth, b'[');
        bytes.extend(std::iter::repeat_n(b']', depth));
        bytes
    }

    #[test]
    fn small_document_round_trips() {
        let encoded = br#"{"ok":true,"n":1,"s":"hi","xs":[null]}"#;
        let value = decode_value(encoded, &JsonDecodeBudget::control_plane()).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["n"], 1);
        assert_eq!(value["s"], "hi");
        assert_eq!(value["xs"][0], Value::Null);
    }

    #[test]
    fn empty_object_array_is_rejected_by_node_budget() {
        let budget = JsonDecodeBudget {
            max_nodes: 8,
            ..JsonDecodeBudget::control_plane()
        };
        // 1 个数组节点 + 8 个对象 = 9，超过 8。
        let error = decode_value(&empty_object_array(8), &budget).unwrap_err();
        assert!(
            matches!(error, JsonDecodeError::Nodes { nodes: 9, max: 8 }),
            "{error:?}"
        );
    }

    #[test]
    fn frame_sized_empty_object_array_hits_data_plane_node_cap() {
        let budget = JsonDecodeBudget::for_frame_bytes(1024);
        // 1024/8 = 128 节点；200 个空对象加数组本身远超该上限，但仍远小于帧长。
        let encoded = empty_object_array(200);
        assert!(encoded.len() < 1024, "the bomb must still fit the frame");
        let error = decode_value(&encoded, &budget).unwrap_err();
        assert!(matches!(error, JsonDecodeError::Nodes { .. }), "{error:?}");
    }

    #[test]
    fn depth_budget_fails_before_serde_recursion_limit() {
        let budget = JsonDecodeBudget {
            max_depth: 4,
            ..JsonDecodeBudget::control_plane()
        };
        let error = decode_value(&nested_arrays(5), &budget).unwrap_err();
        assert!(
            matches!(error, JsonDecodeError::Depth { depth: 5, max: 4 }),
            "{error:?}"
        );
    }

    #[test]
    fn per_string_and_total_string_budgets_are_enforced() {
        let per_string = JsonDecodeBudget {
            max_string_bytes: 4,
            ..JsonDecodeBudget::control_plane()
        };
        let error = decode_value(br#""abcde""#, &per_string).unwrap_err();
        assert!(matches!(
            error,
            JsonDecodeError::StringBytes { bytes: 5, max: 4 }
        ));

        let total = JsonDecodeBudget {
            max_string_bytes: 8,
            max_total_string_bytes: 6,
            ..JsonDecodeBudget::control_plane()
        };
        let error = decode_value(br#"["aaaa","bbb"]"#, &total).unwrap_err();
        assert!(matches!(error, JsonDecodeError::TotalStringBytes { .. }));
    }

    #[test]
    fn array_and_object_width_are_enforced() {
        let arrays = JsonDecodeBudget {
            max_array_len: 2,
            max_nodes: 64,
            ..JsonDecodeBudget::control_plane()
        };
        let error = decode_value(br#"[1,2,3]"#, &arrays).unwrap_err();
        assert!(matches!(
            error,
            JsonDecodeError::ArrayLen { len: 3, max: 2 }
        ));

        let objects = JsonDecodeBudget {
            max_object_keys: 1,
            max_nodes: 64,
            ..JsonDecodeBudget::control_plane()
        };
        let error = decode_value(br#"{"a":1,"b":2}"#, &objects).unwrap_err();
        assert!(matches!(
            error,
            JsonDecodeError::ObjectKeys { keys: 2, max: 1 }
        ));
    }

    #[test]
    fn object_keys_share_the_string_budget() {
        let budget = JsonDecodeBudget {
            max_string_bytes: 2,
            ..JsonDecodeBudget::control_plane()
        };
        let error = decode_value(br#"{"abcd":1}"#, &budget).unwrap_err();
        assert!(matches!(
            error,
            JsonDecodeError::StringBytes { bytes: 4, max: 2 }
        ));
    }

    #[test]
    fn invalid_utf8_and_trailing_junk_are_rejected() {
        assert!(matches!(
            decode_value(&[0xff], &JsonDecodeBudget::control_plane()),
            Err(JsonDecodeError::InvalidUtf8)
        ));
        let error = decode_value(b"true false", &JsonDecodeBudget::control_plane()).unwrap_err();
        assert!(matches!(error, JsonDecodeError::Syntax(_)));
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Tiny {
        ok: bool,
    }

    #[test]
    fn typed_projection_runs_after_the_budget() {
        let parsed: Tiny =
            from_slice_bounded(br#"{"ok":true}"#, &JsonDecodeBudget::control_plane()).unwrap();
        assert_eq!(parsed, Tiny { ok: true });

        let type_error =
            from_slice_bounded::<Tiny>(br#"{"ok":1}"#, &JsonDecodeBudget::control_plane())
                .unwrap_err();
        assert!(matches!(type_error, JsonDecodeError::Type(_)));

        let bomb = from_slice_bounded::<Tiny>(
            &empty_object_array(64),
            &JsonDecodeBudget {
                max_nodes: 8,
                ..JsonDecodeBudget::control_plane()
            },
        )
        .unwrap_err();
        assert!(matches!(bomb, JsonDecodeError::Nodes { .. }));
    }
}
