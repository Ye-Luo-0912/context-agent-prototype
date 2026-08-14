//! RFC 8785 JSON Canonicalization Scheme (JCS).
//!
//! 输出是无空白、对象键按 UTF-16 码元排序、字符串与数字遵循 ECMA-262
//! `JSON.stringify` / `NumberToString` 的 UTF-8。`ArgumentDigest::from_json`
//! 哈希这份字节，而不是 `serde_json::to_vec` 的 Rust 私有形状。
//! `serde_json::Value` 已是合法 UTF-8 且无数 NaN/Inf，因此序列化失败只会出现
//! 在防御性非有限数字路径上。

use std::fmt;

use serde_json::{Map, Number, Value};

/// JCS 序列化失败。合法 `Value` 上不应出现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JcsError {
    reason: &'static str,
}

impl JcsError {
    fn new(reason: &'static str) -> Self {
        Self { reason }
    }

    pub fn reason(&self) -> &'static str {
        self.reason
    }
}

impl fmt::Display for JcsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.reason)
    }
}

impl std::error::Error for JcsError {}

/// 把 JSON 值写成 RFC 8785 规范字节（UTF-8 文本）。
pub fn serialize(value: &Value) -> Result<String, JcsError> {
    let mut out = String::new();
    write_value(&mut out, value)?;
    Ok(out)
}

fn write_value(out: &mut String, value: &Value) -> Result<(), JcsError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => write_number(out, number)?,
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_value(out, item)?;
            }
            out.push(']');
        }
        Value::Object(object) => write_object(out, object)?,
    }
    Ok(())
}

fn write_object(out: &mut String, object: &Map<String, Value>) -> Result<(), JcsError> {
    let mut keys: Vec<&String> = object.keys().collect();
    keys.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    out.push('{');
    for (index, key) in keys.into_iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        write_string(out, key);
        out.push(':');
        write_value(out, &object[key])?;
    }
    out.push('}');
    Ok(())
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{0009}' => out.push_str("\\t"),
            '\u{000A}' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\u{000D}' => out.push_str("\\r"),
            ch if ('\u{0000}'..='\u{001F}').contains(&ch) => {
                let code = u32::from(ch);
                out.push_str("\\u00");
                out.push(hex_digit((code >> 4) as u8));
                out.push(hex_digit((code & 0x0f) as u8));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn hex_digit(nibble: u8) -> char {
    char::from(b"0123456789abcdef"[nibble as usize])
}

fn write_number(out: &mut String, number: &Number) -> Result<(), JcsError> {
    let Some(value) = number.as_f64() else {
        return Err(JcsError::new("JSON number is not an IEEE 754 double"));
    };
    if !value.is_finite() {
        return Err(JcsError::new("NaN and Infinity are not permitted in JCS"));
    }
    out.push_str(&es_number_to_string(value));
    Ok(())
}

/// ECMA-262 `NumberToString`（含 Note 2）。Ryu 给出最短有效数字，再按 V8
/// 的小数点位置规则排版，使 `1e+30` / `0.000001` 一类形式与 RFC 8785 一致。
fn es_number_to_string(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    let negative = value.is_sign_negative();
    let abs = if negative { -value } else { value };
    let mut buffer = ryu::Buffer::new();
    let printed = buffer.format_finite(abs);
    let formatted = match printed.split_once('e') {
        Some((mantissa, exponent)) => scientific_from_ryu(mantissa, exponent),
        None => decimal_from_ryu(printed),
    };
    if negative {
        format!("-{formatted}")
    } else {
        formatted
    }
}

fn decimal_from_ryu(printed: &str) -> String {
    if let Some(stripped) = printed.strip_suffix(".0") {
        stripped.to_owned()
    } else {
        printed.to_owned()
    }
}

fn scientific_from_ryu(mantissa: &str, exponent: &str) -> String {
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|ch| *ch != '.').collect();
    let k = digits.len() as i32;
    // `point` = 小数点在最短数字串中的位置（V8 DoubleToCString）。
    let point = exponent + 1;
    if (0..=21).contains(&point) && point >= k {
        let mut out = digits;
        for _ in 0..(point - k) {
            out.push('0');
        }
        return out;
    }
    if (-6..0).contains(&point) {
        let mut out = String::from("0.");
        for _ in 0..(-point) {
            out.push('0');
        }
        out.push_str(&digits);
        return trim_trailing_zeros(out);
    }
    if (0..=21).contains(&point) && point < k {
        let split = point as usize;
        let mut out = String::new();
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
        return trim_trailing_zeros(out);
    }
    let exp = point - 1;
    let mut out = String::new();
    out.push(digits.as_bytes()[0] as char);
    if digits.len() > 1 {
        out.push('.');
        out.push_str(&digits[1..]);
        out = trim_trailing_zeros(out);
        if out.ends_with('.') {
            out.pop();
        }
    }
    out.push('e');
    if exp >= 0 {
        out.push('+');
    }
    out.push_str(&exp.to_string());
    out
}

fn trim_trailing_zeros(mut value: String) -> String {
    if value.contains('.') {
        while value.ends_with('0') {
            value.pop();
        }
        if value.ends_with('.') {
            value.pop();
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rfc8785_sample_object_is_canonical() {
        let value = json!({
            "numbers": [333333333.33333329, 1E30, 4.50, 2e-3, 1e-27],
            "string": "€$\u{000F}\nA'B\"\\\\\"/",
            "literals": [null, true, false]
        });
        assert_eq!(
            serialize(&value).unwrap(),
            "{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\"string\":\"€$\\u000f\\nA'B\\\"\\\\\\\\\\\"/\"}"
        );
    }

    #[test]
    fn object_keys_sort_by_utf16_code_units() {
        let value = json!({"b": 1, "a": 2});
        assert_eq!(serialize(&value).unwrap(), "{\"a\":2,\"b\":1}");
    }

    #[test]
    fn key_order_does_not_change_the_digest_bytes() {
        let left = serialize(&json!({"a": 1, "nested": {"x": 2, "y": 3}})).unwrap();
        let right = serialize(&json!({"nested": {"y": 3, "x": 2}, "a": 1})).unwrap();
        assert_eq!(left, right);
    }

    #[test]
    fn appendix_b_number_samples() {
        let cases = [
            (0x0000_0000_0000_0000, "0"),
            (0x8000_0000_0000_0000, "0"),
            (0x0000_0000_0000_0001, "5e-324"),
            (0x8000_0000_0000_0001, "-5e-324"),
            (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
            (0xffef_ffff_ffff_ffff, "-1.7976931348623157e+308"),
            (0x4340_0000_0000_0000, "9007199254740992"),
            (0xc340_0000_0000_0000, "-9007199254740992"),
            (0x44b5_2d02_c7e1_4af6, "1e+23"),
            (0x44b5_2d02_c7e1_4af7, "1.0000000000000001e+23"),
            (0x444b_1ae4_d6e2_ef50, "1e+21"),
            (0x3eb0_c6f7_a0b5_ed8d, "0.000001"),
        ];
        for (bits, expected) in cases {
            let value = Value::Number(Number::from_f64(f64::from_bits(bits)).unwrap());
            assert_eq!(serialize(&value).unwrap(), expected, "bits {bits:016x}");
        }
    }
}
