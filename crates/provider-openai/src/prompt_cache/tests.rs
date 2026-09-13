use super::*;
use crate::{
    OpenAiConfig, OpenAiProvider, SamplingPolicy, build_chat_wire_request,
    build_responses_wire_request, wire_names::ToolNameCodec,
};
use agent_contracts::{
    CacheWritePolicy, ModelInput, ModelMessage, ModelRequest, PromptLayout, TurnFrame,
};
use serde_json::{Value, json};
use std::time::Duration;

fn config(protocol: OpenAiProtocol) -> OpenAiConfig {
    OpenAiConfig {
        api_key: "not-a-secret".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        model: "unknown-model-alias".into(),
        protocol,
        max_output_tokens: 128,
        timeout: Duration::from_secs(5),
        send_stream_options: true,
        send_max_tokens: true,
        max_stream_bytes: 4096,
        context_window: Some(32000),
        sampling: SamplingPolicy::ProviderDefault,
    }
}

fn request() -> ModelRequest {
    ModelInput {
        layout: PromptLayout::CurrentStateLast,
        // Responses drops this empty message. The mapping must follow the
        // contract message index, not assume identical wire-array indexes.
        system_policy: vec![ModelMessage::system(""), ModelMessage::system("policy")],
        context_frame: vec![ModelMessage::user("original\n证据")],
        focus_frame: Some("current goal".into()),
        turn_frame: TurnFrame::new("full user directive"),
        ..Default::default()
    }
    .into_request(json!({}), Default::default())
}

fn wire(request: &ModelRequest, mode: OpenAiPromptCacheMode) -> Value {
    build_responses_wire_request(
        request,
        &config(OpenAiProtocol::Responses),
        &ToolNameCodec::from_request(request).unwrap(),
        mode,
    )
}

#[test]
fn explicit_mode_maps_only_the_valid_boundary_and_preserves_all_roles_and_text() {
    let request = request();
    let ordinary = wire(&request, OpenAiPromptCacheMode::ProviderDefault);
    let mut cached = wire(&request, OpenAiPromptCacheMode::ResponsesExplicit);
    assert_eq!(cached["input"][1]["role"], "user");
    assert_eq!(
        cached["input"][1]["content"][0]["prompt_cache_breakpoint"],
        json!({"mode":"explicit"})
    );
    assert_eq!(cached["prompt_cache_options"], json!({"mode":"explicit"}));
    assert_eq!(
        cached["input"].as_array().unwrap().last().unwrap()["role"],
        "system"
    );
    assert_eq!(
        cached["input"].as_array().unwrap().last().unwrap()["content"],
        "current goal"
    );
    assert_eq!(cached["store"], false);
    cached["input"][1]["content"] = cached["input"][1]["content"][0]["text"].clone();
    cached
        .as_object_mut()
        .unwrap()
        .remove("prompt_cache_options");
    assert_eq!(cached, ordinary);
}

#[test]
fn default_unknown_and_stale_hints_keep_historical_wire_bytes() {
    let request = request();
    let mut no_hint = request.clone();
    no_hint.metadata = json!({});
    let default_wire = wire(&no_hint, OpenAiPromptCacheMode::ProviderDefault);
    assert_eq!(
        wire(&request, OpenAiPromptCacheMode::ProviderDefault).to_string(),
        default_wire.to_string()
    );
    assert_eq!(
        wire(&no_hint, OpenAiPromptCacheMode::ResponsesExplicit),
        default_wire
    );
    let mut stale = request.clone();
    stale.messages[2].content = "updated evidence".into();
    assert_eq!(
        wire(&stale, OpenAiPromptCacheMode::ResponsesExplicit),
        wire(&stale, OpenAiPromptCacheMode::ProviderDefault)
    );
    let chat_config = config(OpenAiProtocol::ChatCompletions);
    let codec = ToolNameCodec::from_request(&request).unwrap();
    assert_eq!(
        build_chat_wire_request(&request, &chat_config, &codec),
        build_chat_wire_request(&no_hint, &chat_config, &codec)
    );
}

#[test]
fn explicit_capability_requires_pinned_responses_without_alias_guessing() {
    assert!(OpenAiPromptCacheMode::parse("implicit").is_err());
    for protocol in [OpenAiProtocol::Auto, OpenAiProtocol::ChatCompletions] {
        assert!(
            OpenAiProvider::new(config(protocol))
                .with_prompt_cache_mode(OpenAiPromptCacheMode::ResponsesExplicit)
                .is_err()
        );
    }
    assert!(
        OpenAiProvider::new(config(OpenAiProtocol::Responses))
            .with_prompt_cache_mode(OpenAiPromptCacheMode::ResponsesExplicit)
            .is_ok()
    );
}

// ---------------------------------------------------------------------------
// C1 (R2): the stable prompt-cache routing key reaches the actual wire only
// for the confirmed-capability profile, byte-stable across builds, and never
// changes payloads of unconfirmed endpoints.
// ---------------------------------------------------------------------------

/// The wire must expose the caller's stable routing key in the Responses
/// `prompt_cache_key` field, and two builds of the same request must serialize
/// to byte-identical payloads: the key is the caller's namespace, and any
/// per-build drift would split the cache namespace invisibly.
#[test]
fn confirmed_explicit_profile_sends_a_byte_stable_prompt_cache_key() {
    let mut request = request();
    request.prompt_cache_key = Some("iso:tenant-a|ws:7f3c|task:42|lane:main".into());

    let first = wire(&request, OpenAiPromptCacheMode::ResponsesExplicit);
    let second = wire(&request, OpenAiPromptCacheMode::ResponsesExplicit);
    assert_eq!(
        first.to_string(),
        second.to_string(),
        "two builds of one request must be byte-identical"
    );
    assert_eq!(
        first["prompt_cache_key"], "iso:tenant-a|ws:7f3c|task:42|lane:main",
        "the key travels in the top-level Responses routing field"
    );

    // Same task, next round (only the changing tail differs): the key is
    // stable by construction — the caller keeps it, the transport must not
    // derive anything per request.
    let mut next_round = request.clone();
    next_round.messages.last_mut().unwrap().content = "current goal: verify".into();
    assert_eq!(
        wire(&next_round, OpenAiPromptCacheMode::ResponsesExplicit)["prompt_cache_key"],
        first["prompt_cache_key"]
    );

    // A different isolation domain gets a different namespace; with the key
    // field removed the payloads are identical, so only the key separates
    // the namespaces.
    let mut other_domain = request.clone();
    other_domain.prompt_cache_key = Some("iso:tenant-b|ws:7f3c|task:42|lane:main".into());
    let other_wire = wire(&other_domain, OpenAiPromptCacheMode::ResponsesExplicit);
    assert_ne!(other_wire["prompt_cache_key"], first["prompt_cache_key"]);
    let mut without_key = first.clone();
    without_key
        .as_object_mut()
        .unwrap()
        .remove("prompt_cache_key");
    let mut other_without_key = other_wire.clone();
    other_without_key
        .as_object_mut()
        .unwrap()
        .remove("prompt_cache_key");
    assert_eq!(without_key, other_without_key);
}

/// An endpoint without the confirmed capability must never receive the
/// provider-specific routing field, and a request without a key must keep the
/// exact historical payload bytes.
#[test]
fn unconfirmed_endpoints_and_keyless_requests_keep_historical_payloads() {
    let mut keyed = request();
    keyed.prompt_cache_key = Some("iso:tenant-a|task:42".into());

    // ProviderDefault mode = no confirmed capability: payload unchanged.
    let default_wire = wire(&keyed, OpenAiPromptCacheMode::ProviderDefault);
    assert!(default_wire.get("prompt_cache_key").is_none());

    // No key at all: identical to the historical payload on the confirmed
    // profile too.
    let keyless = request();
    let explicit_keyless = wire(&keyless, OpenAiPromptCacheMode::ResponsesExplicit);
    assert!(explicit_keyless.get("prompt_cache_key").is_none());

    // The Chat dialect has no confirmed cache-key capability signal in this
    // adapter: it never receives the field, with or without a key.
    let chat_config = config(OpenAiProtocol::ChatCompletions);
    let codec = ToolNameCodec::from_request(&keyed).unwrap();
    let chat_keyed = build_chat_wire_request(&keyed, &chat_config, &codec);
    let chat_keyless = build_chat_wire_request(&keyless, &chat_config, &codec);
    assert!(chat_keyed.get("prompt_cache_key").is_none());
    assert_eq!(chat_keyed.to_string(), chat_keyless.to_string());
}

// ---------------------------------------------------------------------------
// C2 (R3): an explicit-only write policy without a valid boundary must send
// the endpoint's supported no-cache-write shape instead of silently falling
// back to the provider-default implicit write.
// ---------------------------------------------------------------------------

/// Explicit-only + no valid boundary (absent or stale hint): the confirmed
/// explicit endpoint receives explicit mode with ZERO breakpoints, which
/// writes nothing — instead of the historical ProviderDefault-shaped request
/// that lets the provider implicitly cache the one-shot suffix.
#[test]
fn explicit_only_policy_without_a_valid_boundary_sends_the_no_write_shape() {
    // Absent hint.
    let mut no_hint_request = request();
    no_hint_request.metadata = json!({});
    no_hint_request.cache_write_policy = Some(CacheWritePolicy::ExplicitOnly);
    let no_boundary = wire(&no_hint_request, OpenAiPromptCacheMode::ResponsesExplicit);
    assert_eq!(
        no_boundary["prompt_cache_options"],
        json!({"mode": "explicit"}),
        "explicit-only keeps the request inside the explicit regime"
    );
    for item in no_boundary["input"].as_array().unwrap() {
        let content = &item["content"];
        let as_array = content.as_array();
        if let Some(blocks) = as_array {
            for block in blocks {
                assert!(
                    block.get("prompt_cache_breakpoint").is_none(),
                    "a no-cache-write request must not declare any breakpoint: {block}"
                );
            }
        }
    }

    // Stale hint: the boundary no longer validates, so it must not be used.
    let mut stale = request();
    stale.messages[2].content = "updated evidence".into();
    stale.cache_write_policy = Some(CacheWritePolicy::ExplicitOnly);
    let stale_wire = wire(&stale, OpenAiPromptCacheMode::ResponsesExplicit);
    assert_eq!(
        stale_wire["prompt_cache_options"],
        json!({"mode": "explicit"})
    );
    assert!(
        !stale_wire.to_string().contains("prompt_cache_breakpoint"),
        "a stale boundary must not become a breakpoint: {stale_wire}"
    );

    // With a VALID boundary the policy writes at that breakpoint as usual.
    let mut with_boundary = request();
    with_boundary.cache_write_policy = Some(CacheWritePolicy::ExplicitOnly);
    let write_wire = wire(&with_boundary, OpenAiPromptCacheMode::ResponsesExplicit);
    assert_eq!(
        write_wire["input"][1]["content"][0]["prompt_cache_breakpoint"],
        json!({"mode": "explicit"})
    );
    assert_eq!(
        write_wire["prompt_cache_options"],
        json!({"mode": "explicit"})
    );
}

/// The policy is a caller expectation, never a capability switch: endpoints
/// without the confirmed explicit mode keep their historical payload bytes,
/// with or without the policy, in both dialects.
#[test]
fn write_policy_never_changes_unconfirmed_or_default_payloads() {
    let mut policy_request = request();
    policy_request.cache_write_policy = Some(CacheWritePolicy::ExplicitOnly);

    // ProviderDefault mode: identical bytes with and without the policy.
    let plain = request();
    assert_eq!(
        wire(&policy_request, OpenAiPromptCacheMode::ProviderDefault).to_string(),
        wire(&plain, OpenAiPromptCacheMode::ProviderDefault).to_string(),
    );

    // Chat dialect: no confirmed capability, identical bytes.
    let chat_config = config(OpenAiProtocol::ChatCompletions);
    let codec = ToolNameCodec::from_request(&policy_request).unwrap();
    assert_eq!(
        build_chat_wire_request(&policy_request, &chat_config, &codec).to_string(),
        build_chat_wire_request(&plain, &chat_config, &codec).to_string(),
    );

    // Confirmed profile WITHOUT the policy: the historical fallback stays
    // exactly as the existing tests pin it (no options field when the hint
    // is missing).
    let mut policyless_no_hint = request();
    policyless_no_hint.metadata = json!({});
    assert!(
        wire(
            &policyless_no_hint,
            OpenAiPromptCacheMode::ResponsesExplicit
        )
        .get("prompt_cache_options")
        .is_none(),
        "the policy-less fallback behavior is unchanged"
    );
}

#[tokio::test]
async fn actual_http_send_keeps_the_breakpoint_before_a_changed_state() {
    use agent_contracts::ModelTransport;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 1024];
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(index) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let header = String::from_utf8_lossy(&bytes[..header_end]);
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + length {
                let mut chunk = [0u8; 1024];
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
            }
            requests.push(
                serde_json::from_slice::<Value>(&bytes[header_end..header_end + length]).unwrap(),
            );
            let body = "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
        requests
    });
    let mut config = config(OpenAiProtocol::Responses);
    config.base_url = format!("http://{address}/v1");
    let provider = OpenAiProvider::with_client(
        config,
        reqwest::Client::builder().no_proxy().build().unwrap(),
    )
    .with_prompt_cache_mode(OpenAiPromptCacheMode::ResponsesExplicit)
    .unwrap()
    .with_responses_reasoning_effort(crate::ResponsesReasoningEffort::None)
    .unwrap();
    let first = request();
    let mut changed = first.clone();
    changed.messages.last_mut().unwrap().content = "new current goal".into();
    for request in [first, changed] {
        assert_eq!(provider.complete(request).await.unwrap().content, "ok");
    }
    let sent = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sent[0]["input"][1], sent[1]["input"][1]);
    assert_eq!(
        sent[0]["input"][1]["content"][0]["prompt_cache_breakpoint"]["mode"],
        "explicit"
    );
    assert_eq!(
        sent[1]["input"].as_array().unwrap().last().unwrap()["content"],
        "new current goal"
    );
    assert_eq!(
        sent[0]["prompt_cache_options"],
        sent[1]["prompt_cache_options"]
    );
    assert_eq!(sent[0]["reasoning"], json!({"effort":"none"}));
    assert_eq!(sent[1]["reasoning"], sent[0]["reasoning"]);
}

/// Spawns a loopback server that answers `count` Responses calls with a
/// minimal valid stream and returns every captured JSON payload.
async fn serve_response_payloads(
    count: usize,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<Value>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut payloads = Vec::new();
        for _ in 0..count {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(index) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let header = String::from_utf8_lossy(&bytes[..header_end]);
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while bytes.len() < header_end + length {
                let mut chunk = [0u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&chunk[..read]);
            }
            payloads.push(
                serde_json::from_slice::<Value>(&bytes[header_end..header_end + length]).unwrap(),
            );
            let body = "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        }
        payloads
    });
    (address, server)
}

fn explicit_provider(base_url: String) -> OpenAiProvider {
    let mut provider_config = config(OpenAiProtocol::Responses);
    provider_config.base_url = base_url;
    OpenAiProvider::with_client(
        provider_config,
        reqwest::Client::builder().no_proxy().build().unwrap(),
    )
    .with_prompt_cache_mode(OpenAiPromptCacheMode::ResponsesExplicit)
    .unwrap()
    .with_responses_reasoning_effort(crate::ResponsesReasoningEffort::None)
    .unwrap()
}

/// C1 stop condition: the stable routing key is visible in the ACTUAL HTTP
/// payload, identical across the consecutive requests of one task, and
/// distinct across isolation domains.
#[tokio::test]
async fn actual_http_payloads_carry_the_stable_cache_key_per_isolation_domain() {
    use agent_contracts::ModelTransport;
    let (address, server) = serve_response_payloads(3).await;
    let provider = explicit_provider(format!("http://{address}/v1"));

    let mut first = request();
    first.prompt_cache_key = Some("iso:tenant-a|task:42|lane:main".into());
    let mut next_round = first.clone();
    next_round.messages.last_mut().unwrap().content = "next round goal".into();
    let mut other_domain = first.clone();
    other_domain.prompt_cache_key = Some("iso:tenant-b|task:42|lane:main".into());

    for request in [first, next_round, other_domain] {
        assert_eq!(provider.complete(request).await.unwrap().content, "ok");
    }
    let sent = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        sent[0]["prompt_cache_key"],
        "iso:tenant-a|task:42|lane:main"
    );
    assert_eq!(
        sent[1]["prompt_cache_key"], sent[0]["prompt_cache_key"],
        "one task's consecutive requests share the routing key"
    );
    assert_eq!(
        sent[2]["prompt_cache_key"], "iso:tenant-b|task:42|lane:main",
        "a different isolation domain never reuses the namespace"
    );
}

/// C2 stop condition: on the confirmed explicit-only endpoint, a call with no
/// valid boundary reaches the wire as the zero-breakpoint explicit shape (no
/// cache write), while a valid boundary still writes at its breakpoint.
#[tokio::test]
async fn actual_http_payload_honors_the_explicit_only_no_write_shape() {
    use agent_contracts::ModelTransport;
    let (address, server) = serve_response_payloads(2).await;
    let provider = explicit_provider(format!("http://{address}/v1"));

    let mut no_boundary = request();
    no_boundary.metadata = json!({});
    no_boundary.cache_write_policy = Some(CacheWritePolicy::ExplicitOnly);
    let mut with_boundary = request();
    with_boundary.cache_write_policy = Some(CacheWritePolicy::ExplicitOnly);

    for request in [no_boundary, with_boundary] {
        assert_eq!(provider.complete(request).await.unwrap().content, "ok");
    }
    let sent = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        sent[0]["prompt_cache_options"],
        json!({"mode": "explicit"}),
        "the no-boundary call stays inside the explicit regime"
    );
    assert!(
        !sent[0].to_string().contains("prompt_cache_breakpoint"),
        "the no-boundary call must not write any cache breakpoint: {}",
        sent[0]
    );
    assert_eq!(
        sent[1]["input"][1]["content"][0]["prompt_cache_breakpoint"]["mode"], "explicit",
        "a valid boundary still writes at its breakpoint"
    );
}
