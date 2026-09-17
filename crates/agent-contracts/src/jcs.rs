//! RFC 8785 JSON Canonicalization Scheme (JCS).
//!
//! 输出是无空白、对象键按 UTF-16 码元排序、字符串与数字遵循 ECMA-262
//! `JSON.stringify` / `NumberToString` 的 UTF-8。`ArgumentDigest::from_json`
//! 哈希这份字节，而不是 `serde_json::to_vec` 的 Rust 私有形状。
//! `serde_json::Value` 已是合法 UTF-8 且无数 NaN/Inf，因此序列化失败只会出现
//! 在防御性非有限数字路径上。
//!
//! 数值域：按 RFC 8785 §3.1，`write_number` 经由 binary64 渲染每个数字；
//! 这对 I-JSON 输入是无损的。参数准入（`schema_profile`）负责把工具参数
//! 限制在同一 binary64 域内（整数 |n| ≤ 2^53），使「通过校验的参数」的
//! 规范化字节与其执行语义完全一致；本模块不做域判断，也不静默扩大它。

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
    // `point` = 小数点在最短数字串中的位置，即 ECMA-262
    // `Number::toString` 的 n。四个分支与该算法（RFC 8785 §3.2.2.3 引用）
    // 一一对应，开闭边界不可调换：plain 形式要求 k ≤ n ≤ 21 与 0 < n < k，
    // 前导零形式要求 -6 < n ≤ 0（n = -6 的 1e-7 一带必须保持指数形式）。
    let point = exponent + 1;
    if (k..=21).contains(&point) {
        // k ≤ n ≤ 21：数字后补零。
        let mut out = digits;
        for _ in 0..(point - k) {
            out.push('0');
        }
        return out;
    }
    if (1..=21).contains(&point) {
        // 0 < n < k：小数点落在数字串内。
        let split = point as usize;
        let mut out = String::new();
        out.push_str(&digits[..split]);
        out.push('.');
        out.push_str(&digits[split..]);
        return trim_trailing_zeros(out);
    }
    if (-5..=0).contains(&point) {
        // -6 < n ≤ 0：前缀 0. 补零。
        let mut out = String::from("0.");
        for _ in 0..(-point) {
            out.push('0');
        }
        out.push_str(&digits);
        return trim_trailing_zeros(out);
    }
    // 其余（n ≤ -6 或 n > 21）：指数形式，exp = n - 1。
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
        // RFC 8785 appendix sample: the extra digit is the spec's input, not
        // a rounding we should "fix" in the fixture.
        #[allow(clippy::excessive_precision)]
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

    /// Full RFC 8785 Appendix B vector set, expected values cross-checked
    /// against ECMAScript `Number::toString` (Node v24, this machine).
    #[test]
    fn rfc8785_appendix_b_vectors() {
        let cases = [
            (0x0000_0000_0000_0000, "0"),
            (0x8000_0000_0000_0000, "0"),
            (0x0000_0000_0000_0001, "5e-324"),
            (0x8000_0000_0000_0001, "-5e-324"),
            (0x0000_0000_0000_0002, "1e-323"),
            (0x8000_0000_0000_0002, "-1e-323"),
            (0x0000_0000_0000_000f, "7.4e-323"),
            (0x8000_0000_0000_000f, "-7.4e-323"),
            (0x0000_0000_0000_0010, "8e-323"),
            (0x3ff0_0000_0000_0000, "1"),
            (0x3ff0_0000_0000_0001, "1.0000000000000002"),
            (0x3ff0_0000_0000_0002, "1.0000000000000004"),
            (0x4000_0000_0000_0000, "2"),
            (0x4008_0000_0000_0000, "3"),
            (0x400c_0000_0000_0000, "3.5"),
            (0x4010_0000_0000_0000, "4"),
            (0x4014_0000_0000_0000, "5"),
            (0x4020_0000_0000_0000, "8"),
            (0x4024_0000_0000_0000, "10"),
            (0x4059_0000_0000_0000, "100"),
            (0x40ac_2000_0000_0000, "3600"),
            (0x4462_0000_0000_0000, "2.6563311466141754e+21"),
            (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
            (0xffef_ffff_ffff_ffff, "-1.7976931348623157e+308"),
            (0x0ffe_ffff_ffff_ffff, "1.2479725741094444e-231"),
            (0x4340_0000_0000_0000, "9007199254740992"),
            (0xc340_0000_0000_0000, "-9007199254740992"),
            (0x44b5_2d02_c7e1_4af6, "1e+23"),
            (0x44b5_2d02_c7e1_4af7, "1.0000000000000001e+23"),
            (0x444b_1ae4_d6e2_ef50, "1e+21"),
            (0x3eb0_c6f7_a0b5_ed8d, "0.000001"),
            (0x41b3_de43_5555_5553, "333333333.3333332"),
        ];
        for (bits, expected) in cases {
            let value = Value::Number(Number::from_f64(f64::from_bits(bits)).unwrap());
            assert_eq!(serialize(&value).unwrap(), expected, "bits {bits:016x}");
        }
    }

    /// RFC 8785 §3.2.2.3 (ECMA-262 `Number::toString`): the zero-prefixed
    /// plain-decimal form requires -6 < n ≤ 0, where n is the decimal point
    /// position in the shortest digit string. n = -6 — the 1e-7 band — must
    /// keep the exponential form. The old `(-6..0)` window accepted n = -6,
    /// so `1e-7` was rewritten to `0.0000001` and its ArgumentDigest hashed
    /// bytes no other JCS implementation produces. Expected values are Node
    /// `JSON.stringify` output (this machine, v24).
    #[test]
    fn sub_unit_numbers_stay_scientific_across_the_1e6_threshold() {
        let cases = [
            (1e-7, "1e-7"),
            (1.2e-7, "1.2e-7"),
            (-1e-7, "-1e-7"),
            (-1.2e-7, "-1.2e-7"),
            (1.5e-7, "1.5e-7"),
            (9.9e-7, "9.9e-7"),
            (1e-8, "1e-8"),
            (1e-20, "1e-20"),
            // Legal controls: inside the plain-decimal band.
            (1e-6, "0.000001"),
            (1.5e-6, "0.0000015"),
            (1e-5, "0.00001"),
            (0.5, "0.5"),
        ];
        for (value, expected) in cases {
            let number = Value::Number(Number::from_f64(value).unwrap());
            assert_eq!(serialize(&number).unwrap(), expected, "value {value:?}");
        }
    }

    /// The upper boundary must stay on the spec side too: plain digits only
    /// while n ≤ 21, exponential from n = 22 (1e21 and above).
    #[test]
    fn exponential_threshold_at_1e21_stays_on_the_spec_side() {
        let cases = [
            (1e20, "100000000000000000000"),
            (9e20, "900000000000000000000"),
            (9.999e20, "999900000000000000000"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (1e22, "1e+22"),
        ];
        for (value, expected) in cases {
            let number = Value::Number(Number::from_f64(value).unwrap());
            assert_eq!(serialize(&number).unwrap(), expected, "value {value:?}");
        }
    }
}
