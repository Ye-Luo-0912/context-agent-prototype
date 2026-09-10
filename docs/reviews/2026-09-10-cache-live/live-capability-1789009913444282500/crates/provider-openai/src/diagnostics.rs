//! Opt-in, per-call diagnostics. These describe our HTTP boundary, never the
//! upstream tokenizer, hidden rendering, routing, or a promise of cache reuse.
use agent_contracts::ContentDigest;
use serde::Serialize;
use serde_json::Value;

const PREFIX_BLOCK_BYTES: usize = 1024;
const MAX_PREFIX_BLOCKS: usize = 256;
const MAX_INPUT_ITEMS: usize = 128;

/// Synchronous callbacks must be short and must not panic. Each observed call
/// supplies its own observer; the provider retains no last-request state.
/// Callbacks contain no headers, endpoint, prompt, tool arguments, or output.
/// Auto negotiation may emit two request observations. A response observation
/// is a reported SSE snapshot, not proof that the call completed successfully.
pub trait OpenAiCallObserver: Send + Sync {
    fn on_request(&self, request: WireRequestObservation);
    fn on_response(&self, response: WireResponseObservation);
}

#[derive(Debug, Clone, Serialize)]
pub struct WireRequestObservation {
    pub protocol: &'static str,
    pub body_bytes: usize,
    pub body_sha256: String,
    /// Consecutive equal blocks give a byte-prefix lower bound, not tokens.
    pub prefix_block_bytes: usize,
    pub prefix_block_sha256: Vec<String>,
    pub prefix_complete: bool,
    pub input_items_total: usize,
    pub input_items: Vec<WireInputObservation>,
    pub input_items_complete: bool,
    pub tools_sha256: String,
    /// All top-level fields other than input/messages/tools (including model).
    pub settings_sha256: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WireInputObservation {
    pub kind: &'static str,
    pub bytes: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WireResponseObservation {
    pub protocol: &'static str,
    pub event: &'static str,
    /// The endpoint's claim; it need not identify the underlying model.
    pub reported_model: Option<String>,
    pub system_fingerprint: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub cache_miss_input_tokens: Option<u64>,
    /// Optional extension fields with invalid shapes or conflicting aliases.
    /// Diagnostic failures never change the normal provider parse behavior.
    pub invalid_fields: Vec<&'static str>,
}

fn digest(bytes: &[u8]) -> String {
    ContentDigest::sha256_bytes(bytes).to_string()
}

fn encoded(value: &Value) -> Vec<u8> {
    // A JSON Value has no fallible custom serializer.
    serde_json::to_vec(value).expect("serialize JSON value")
}

pub(crate) fn observe_request(
    observer: Option<&dyn OpenAiCallObserver>,
    protocol: &'static str,
    body: &[u8],
    payload: &Value,
) {
    let Some(observer) = observer else { return };
    let items = payload
        .get("input")
        .or_else(|| payload.get("messages"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let input_items = items
        .iter()
        .take(MAX_INPUT_ITEMS)
        .map(|item| {
            let bytes = encoded(item);
            let kind = match item.get("type").and_then(Value::as_str) {
                Some("function_call") => "function_call",
                Some("function_call_output") => "function_call_output",
                _ => match item.get("role").and_then(Value::as_str) {
                    Some("system") => "system",
                    Some("developer") => "developer",
                    Some("user") => "user",
                    Some("assistant") => "assistant",
                    Some("tool") => "tool",
                    _ => "other",
                },
            };
            WireInputObservation {
                kind,
                bytes: bytes.len(),
                sha256: digest(&bytes),
            }
        })
        .collect();
    let settings: serde_json::Map<String, Value> = payload
        .as_object()
        .into_iter()
        .flat_map(|object| object.iter())
        .filter(|(key, _)| !matches!(key.as_str(), "input" | "messages" | "tools"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    observer.on_request(WireRequestObservation {
        protocol,
        body_bytes: body.len(),
        body_sha256: digest(body),
        prefix_block_bytes: PREFIX_BLOCK_BYTES,
        prefix_block_sha256: body
            .chunks(PREFIX_BLOCK_BYTES)
            .take(MAX_PREFIX_BLOCKS)
            .map(digest)
            .collect(),
        prefix_complete: body.len() <= PREFIX_BLOCK_BYTES * MAX_PREFIX_BLOCKS,
        input_items_total: items.len(),
        input_items,
        input_items_complete: items.len() <= MAX_INPUT_ITEMS,
        tools_sha256: digest(&encoded(&payload["tools"])),
        settings_sha256: digest(&encoded(&Value::Object(settings))),
    });
}

fn identifier(
    value: Option<&Value>,
    field: &'static str,
    invalid: &mut Vec<&'static str>,
) -> Option<String> {
    let value = value.filter(|v| !v.is_null())?;
    if let Some(text) = value.as_str()
        && !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:/".contains(&b))
    {
        return Some(text.to_owned());
    }
    invalid.push(field);
    None
}

fn count(
    value: Option<&Value>,
    field: &'static str,
    invalid: &mut Vec<&'static str>,
) -> Option<u64> {
    let value = value.filter(|v| !v.is_null())?;
    match value.as_u64() {
        Some(value) => Some(value),
        None => {
            invalid.push(field);
            None
        }
    }
}

pub(crate) fn observe_response(
    observer: Option<&dyn OpenAiCallObserver>,
    protocol: &'static str,
    value: &Value,
) {
    let Some(observer) = observer else { return };
    let (event, response) = if protocol == "responses" {
        match value.get("type").and_then(Value::as_str) {
            Some("response.created") => ("created", &value["response"]),
            Some("response.completed") => ("completed", &value["response"]),
            Some("response.failed") => ("failed", &value["response"]),
            Some("response.incomplete") => ("incomplete", &value["response"]),
            _ => return,
        }
    } else {
        ("chunk", value)
    };
    if response.get("model").is_none()
        && response.get("system_fingerprint").is_none()
        && response.get("usage").is_none()
    {
        return;
    }
    let mut invalid = Vec::new();
    let usage = &response["usage"];
    if !usage.is_null() && !usage.is_object() {
        invalid.push("usage");
    }
    let (input, output, details) = if protocol == "responses" {
        ("input_tokens", "output_tokens", "input_tokens_details")
    } else {
        (
            "prompt_tokens",
            "completion_tokens",
            "prompt_tokens_details",
        )
    };
    if !usage[details].is_null() && !usage[details].is_object() {
        invalid.push(details);
    }
    let standard_hit = count(
        usage[details].get("cached_tokens"),
        "cached_tokens",
        &mut invalid,
    );
    let compatible_hit = count(
        usage.get("prompt_cache_hit_tokens"),
        "prompt_cache_hit_tokens",
        &mut invalid,
    );
    let cache_read_input_tokens = match (standard_hit, compatible_hit) {
        (Some(a), Some(b)) if a != b => {
            invalid.push("conflicting_cache_read_tokens");
            None
        }
        (a, b) => a.or(b),
    };
    observer.on_response(WireResponseObservation {
        protocol,
        event,
        reported_model: identifier(response.get("model"), "model", &mut invalid),
        system_fingerprint: identifier(
            response.get("system_fingerprint"),
            "system_fingerprint",
            &mut invalid,
        ),
        input_tokens: count(usage.get(input), input, &mut invalid),
        output_tokens: count(usage.get(output), output, &mut invalid),
        cache_read_input_tokens,
        cache_write_input_tokens: count(
            usage[details].get("cache_write_tokens"),
            "cache_write_tokens",
            &mut invalid,
        ),
        // A cache miss is not a charged cache write. Preserve both separately.
        cache_miss_input_tokens: count(
            usage.get("prompt_cache_miss_tokens"),
            "prompt_cache_miss_tokens",
            &mut invalid,
        ),
        invalid_fields: invalid,
    });
}

pub(crate) fn observe_chat_response(observer: Option<&dyn OpenAiCallObserver>, data: &str) {
    if observer.is_some()
        && let Ok(value) = serde_json::from_str(data)
    {
        observe_response(observer, "chat_completions", &value);
    }
}

#[cfg(test)]
mod tests;
