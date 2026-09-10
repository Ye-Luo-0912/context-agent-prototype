use super::*;
use crate::{OpenAiConfig, OpenAiProtocol, OpenAiProvider, SamplingPolicy};
use agent_contracts::{ModelMessage, ModelRequest, ModelTransport};
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Recorder {
    requests: Mutex<Vec<WireRequestObservation>>,
    responses: Mutex<Vec<WireResponseObservation>>,
    http_errors: Mutex<Vec<WireHttpErrorObservation>>,
}
impl OpenAiCallObserver for Recorder {
    fn on_http_error(&self, error: WireHttpErrorObservation) {
        self.http_errors.lock().unwrap().push(error);
    }
    fn on_request(&self, request: WireRequestObservation) {
        self.requests.lock().unwrap().push(request);
    }
    fn on_response(&self, response: WireResponseObservation) {
        self.responses.lock().unwrap().push(response);
    }
}

#[test]
fn http_diagnostics_report_only_exact_bounded_cache_rejections() {
    let recorder = Recorder::default();
    let cases = [
        (400, json!({"error":{"message":"prompt_cache_breakpoint is not supported on this model", "type":"upstream_error"}}).to_string(), Some(PromptCacheRejection::BreakpointUnsupportedByModel)),
        (422, json!({"error":{"message":"prompt_cache_options is not supported on this model."}}).to_string(), Some(PromptCacheRejection::OptionsUnsupportedByModel)),
        (429, json!({"error":{"message":"prompt_cache_breakpoint is not supported on this model"}}).to_string(), None),
        (400, json!({"error":{"message":"private text mentioning prompt_cache_breakpoint"}}).to_string(), None),
        (400, "private malformed error".into(), None),
        (400, json!({"error":{"message":"prompt_cache_breakpoint is not supported on this model"},"private":"s".repeat(8192)}).to_string(), None),
    ];
    for (status, body, expected) in cases {
        observe_http_error(Some(&recorder), "responses", status, &body);
        let errors = recorder.http_errors.lock().unwrap();
        let error = errors.last().unwrap();
        assert_eq!(error.status_code, status);
        assert_eq!(error.reported_cache_rejection, expected);
        assert!(!serde_json::to_string(error).unwrap().contains("private"));
    }
}

#[tokio::test]
async fn http_rejection_observation_preserves_the_failure_and_wire_on_both_protocols() {
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                bodies.push(read_body(&mut socket).await);
                let body = r#"{"error":{"message":"prompt_cache_breakpoint is not supported on this model","type":"upstream_error"}}"#;
                socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            bodies
        });
        let provider = OpenAiProvider::with_client(
            OpenAiConfig {
                api_key: "not-a-real-secret".into(),
                base_url: format!("http://{address}/v1"),
                model: "configured-alias".into(),
                protocol,
                max_output_tokens: 64,
                timeout: Duration::from_secs(5),
                send_stream_options: true,
                send_max_tokens: true,
                max_stream_bytes: 64 * 1024,
                context_window: None,
                sampling: SamplingPolicy::ProviderDefault,
            },
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );
        let request = ModelRequest {
            messages: vec![ModelMessage::user("fixture")],
            tools: vec![],
            metadata: json!({}),
            cancel: Default::default(),
        };
        let recorder = Recorder::default();
        let observed = provider
            .complete_stream_observed(request.clone(), &crate::NoopSink, &recorder)
            .await
            .unwrap_err();
        let ordinary = provider.complete(request).await.unwrap_err();
        assert!(matches!(
            observed,
            agent_contracts::AgentError::Transport {
                retryable: false,
                ..
            }
        ));
        assert_eq!(observed.to_string(), ordinary.to_string());
        let bodies = server.await.unwrap();
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(recorder.requests.lock().unwrap().len(), 1);
        assert!(recorder.responses.lock().unwrap().is_empty());
        let errors = recorder.http_errors.lock().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].status_code, 400);
        assert_eq!(
            errors[0].reported_cache_rejection,
            Some(PromptCacheRejection::BreakpointUnsupportedByModel)
        );
    }
}

#[test]
fn request_trace_is_bounded_and_does_not_contain_prompt_or_settings_values() {
    let payload = json!({"input":vec![json!({"role":"user","content":"private-fixture".repeat(200) }); 150],
        "model":"private-model", "tools":[{"name":"private-tool"}], "stream":true});
    let body = encoded(&payload);
    let recorder = Recorder::default();
    observe_request(Some(&recorder), "responses", &body, &payload);
    let requests = recorder.requests.lock().unwrap();
    let trace = &requests[0];
    assert_eq!(trace.body_sha256, digest(&body));
    assert_eq!(trace.body_bytes, body.len());
    assert_eq!(trace.input_items_total, 150);
    assert_eq!(trace.input_items.len(), MAX_INPUT_ITEMS);
    assert_eq!(trace.prefix_block_sha256.len(), MAX_PREFIX_BLOCKS);
    assert!(!trace.prefix_complete);
    assert!(!trace.input_items_complete);
    assert!(!serde_json::to_string(trace).unwrap().contains("private-"));
}

#[test]
fn response_metadata_distinguishes_missing_zero_miss_write_and_conflicts() {
    let recorder = Recorder::default();
    let observer = Some(&recorder as &dyn OpenAiCallObserver);
    observe_response(
        observer,
        "responses",
        &json!({"type":"response.completed","response":{
        "model":"served-model-v2", "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":80}}}}),
    );
    observe_response(
        observer,
        "chat_completions",
        &json!({"model":"alias", "usage":{
        "prompt_tokens":100,"prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20}}),
    );
    observe_response(
        observer,
        "responses",
        &json!({"type":"response.incomplete","response":{
        "model":"untrusted text\n", "usage":{"input_tokens_details":{"cache_write_tokens":-1}}}}),
    );
    observe_response(
        observer,
        "chat_completions",
        &json!({"usage":{
        "prompt_tokens_details":{"cached_tokens":0},"prompt_cache_hit_tokens":80}}),
    );
    let responses = recorder.responses.lock().unwrap();
    assert_eq!(responses[0].cache_read_input_tokens, Some(0));
    assert_eq!(responses[0].cache_write_input_tokens, Some(80));
    assert_eq!(responses[0].output_tokens, None);
    assert_eq!(responses[1].cache_read_input_tokens, Some(80));
    assert_eq!(responses[1].cache_miss_input_tokens, Some(20));
    assert_eq!(responses[1].cache_write_input_tokens, None);
    assert_eq!(responses[2].event, "incomplete");
    assert_eq!(responses[2].reported_model, None);
    assert_eq!(responses[2].cache_write_input_tokens, None);
    assert!(responses[2].invalid_fields.contains(&"cache_write_tokens"));
    assert!(responses[2].invalid_fields.contains(&"model"));
    assert_eq!(responses[3].cache_read_input_tokens, None);
    assert!(
        responses[3]
            .invalid_fields
            .contains(&"conflicting_cache_read_tokens")
    );
}

async fn read_body(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut incoming = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let n = socket.read(&mut buffer).await.unwrap();
        assert_ne!(n, 0);
        incoming.extend_from_slice(&buffer[..n]);
        assert!(incoming.len() <= 64 * 1024);
        if let Some(end) = incoming.windows(4).position(|w| w == b"\r\n\r\n") {
            let end = end + 4;
            let length: usize = std::str::from_utf8(&incoming[..end])
                .unwrap()
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                })
                .unwrap()
                .1
                .trim()
                .parse()
                .unwrap();
            if incoming.len() >= end + length {
                return incoming[end..end + length].to_vec();
            }
        }
    }
}

#[tokio::test]
async fn observer_fingerprints_actual_http_bytes_without_changing_either_protocol() {
    for protocol in [OpenAiProtocol::Responses, OpenAiProtocol::ChatCompletions] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                bodies.push(read_body(&mut socket).await);
                let sse = if protocol == OpenAiProtocol::Responses {
                    concat!(
                        "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"ok\"}\n\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"served-v2\",\"usage\":{\"input_tokens\":20,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":12,\"cache_write_tokens\":4}}}}\n\n"
                    )
                } else {
                    concat!(
                        "data: {\"model\":\"served-v2\",\"system_fingerprint\":\"fp_mock\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":12}}}\n\n",
                        "data: [DONE]\n\n"
                    )
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{sse}",
                    sse.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            bodies
        });
        let provider = OpenAiProvider::with_client(
            OpenAiConfig {
                api_key: "not-a-real-secret".into(),
                base_url: format!("http://{addr}/v1"),
                model: "configured-alias".into(),
                protocol,
                max_output_tokens: 64,
                timeout: Duration::from_secs(5),
                send_stream_options: true,
                send_max_tokens: true,
                max_stream_bytes: 64 * 1024,
                context_window: None,
                sampling: SamplingPolicy::ProviderDefault,
            },
            reqwest::Client::builder().no_proxy().build().unwrap(),
        );
        let request = ModelRequest {
            messages: vec![
                ModelMessage::system("policy"),
                ModelMessage::user("private-evidence"),
                ModelMessage::system("current-focus"),
            ],
            tools: vec![],
            metadata: json!({"private-metadata":"must-not-send"}),
            cancel: Default::default(),
        };
        let recorder = Recorder::default();
        let observed = provider
            .complete_stream_observed(request.clone(), &crate::NoopSink, &recorder)
            .await
            .unwrap();
        let ordinary = provider.complete(request).await.unwrap();
        assert_eq!(observed.content, ordinary.content);
        let bodies = server.await.unwrap();
        assert_eq!(bodies[0], bodies[1]);
        let requests = recorder.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body_sha256, digest(&bodies[0]));
        assert_eq!(
            requests[0]
                .input_items
                .iter()
                .map(|i| i.kind)
                .collect::<Vec<_>>(),
            ["system", "user", "system"]
        );
        let trace = serde_json::to_string(&requests[0]).unwrap();
        assert!(!trace.contains("private-evidence"));
        assert!(!trace.contains("not-a-real-secret"));
        let response = &recorder.responses.lock().unwrap()[0];
        assert_eq!(response.reported_model.as_deref(), Some("served-v2"));
        assert_eq!(
            response.cache_read_input_tokens,
            observed.usage.cached_input_tokens
        );
        assert_eq!(
            response.cache_write_input_tokens,
            (protocol == OpenAiProtocol::Responses).then_some(4)
        );
    }
}
