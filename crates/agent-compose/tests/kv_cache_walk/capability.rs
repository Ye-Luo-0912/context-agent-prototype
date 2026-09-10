use super::*;
use provider_openai::OpenAiPromptCacheMode;

/// Local display only, after removing configured secrets and credential-like
/// tokens. Persisted probe reports still contain no arbitrary error body.
fn redacted_error(error: &AgentError, secrets: &[String]) -> String {
    let mut text = error.to_string();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, "[redacted]");
    }
    text.split_inclusive(|ch: char| !ch.is_ascii_alphanumeric() && !"_-/+=".contains(ch))
        .map(|token| {
            if token.len() > 32 || token.starts_with("sk-") {
                "[redacted]"
            } else {
                token
            }
        })
        .collect::<String>()
        .chars()
        .take(600)
        .collect()
}

#[test]
fn error_display_removes_configured_and_credential_shaped_values() {
    let error = AgentError::Transport { retryable:false,
        message:"HTTP 400: prompt_cache_breakpoint secret-one sk-fake-token abcdefghijklmnopqrstuvwxyz1234567890".into() };
    let display = redacted_error(&error, &["secret-one".into()]);
    assert!(display.contains("prompt_cache_breakpoint"));
    assert!(!display.contains("secret-one"));
    assert!(!display.contains("sk-fake"));
    assert!(!display.contains("abcdefghijklmnopqrstuvwxyz"));
}

#[tokio::test]
#[ignore = "live provider: two tiny shape checks, not a cache-benefit measurement"]
async fn probe_cache_parameter_capability() {
    assert_eq!(std::env::var("KV_CACHE_LIVE").as_deref(), Ok("1"));
    let path = std::path::PathBuf::from(std::env::var("KV_CACHE_REPORT").expect("report required"));
    let (mut provider, mut profile) = live_configuration();
    let secrets: Vec<_> = ["OPENAI_API_KEY", "OPENAI_BASE_URL"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .collect();
    let input = ModelInput {
        layout: PromptLayout::CurrentStateLast,
        system_policy: vec![ModelMessage::system(format!(
            "Synthetic API shape check {}. Return exactly OK.",
            RunId::new()
        ))],
        context_frame: vec![ModelMessage::user("Synthetic data: STATUS=OK.")],
        turn_frame: TurnFrame::new("Copy STATUS."),
        ..Default::default()
    };
    let mut report = json!({"schema":"kv-capability-shape/v2","max_requests":2,
        "scope":"Tiny production-provider field acceptance only, below cache-measurement size",
        "raw_requests_saved":false,"raw_errors_saved":false,"transport_retries":false,"requests":[]});
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    write_report(&path, &report);
    // Validate the extension first: a rejected field can be diagnosed even
    // while ordinary model generation is delayed at the same endpoint.
    for (index, mode) in [
        OpenAiPromptCacheMode::ResponsesExplicit,
        OpenAiPromptCacheMode::ProviderDefault,
    ]
    .into_iter()
    .enumerate()
    {
        provider = provider.with_prompt_cache_mode(mode).unwrap();
        profile.prompt_cache_mode = mode;
        let request = input
            .clone()
            .into_request(json!({}), CancellationToken::new());
        let observer = CallObservation::default();
        let timing = TimingSink {
            start: Instant::now(),
            first_text_ms: AtomicU64::new(u64::MAX),
        };
        report["attempted_requests"] = json!(index + 1);
        write_report(&path, &report);
        let result = provider
            .complete_stream_observed(request, &timing, &observer)
            .await;
        let mut row = json!({"mode":mode.as_str(),"profile_digest":profile.digest(),
            "elapsed_ms":timing.start.elapsed().as_millis() as u64,
            "wire_requests":&*observer.requests.lock().unwrap(),
            "http_errors":&*observer.http_errors.lock().unwrap(),
            "response_observations":&*observer.responses.lock().unwrap()});
        let success = match result {
            Ok(output) => {
                row["completed"] = json!(true);
                row["answer_correct"] = json!(output.content.trim() == "OK");
                row["usage"] = serde_json::to_value(output.usage).unwrap();
                println!("KV_CAPABILITY mode={} completed=true", mode.as_str());
                true
            }
            Err(error) => {
                row["transport_failure"] = boundary::transport_failure_summary(&error);
                row["completed"] = json!(false);
                report["outcome"] = json!(if row["transport_failure"]["http_status"].is_number() {
                    "http_request_rejected"
                } else {
                    "transport_unavailable"
                });
                println!(
                    "KV_CAPABILITY mode={} {}",
                    mode.as_str(),
                    redacted_error(&error, &secrets)
                );
                false
            }
        };
        report["requests"].as_array_mut().unwrap().push(row);
        write_report(&path, &report);
        if !success {
            break;
        }
    }
}
