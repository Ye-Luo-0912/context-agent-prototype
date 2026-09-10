use super::*;
use crate::{
    OpenAiConfig, OpenAiProvider, SamplingPolicy, build_chat_wire_request,
    build_responses_wire_request, wire_names::ToolNameCodec,
};
use agent_contracts::{ModelInput, ModelMessage, ModelRequest, PromptLayout, TurnFrame};
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
