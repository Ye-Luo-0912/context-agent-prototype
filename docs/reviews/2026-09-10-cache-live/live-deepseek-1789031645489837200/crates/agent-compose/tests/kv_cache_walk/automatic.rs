use super::*;
use provider_openai::{OpenAiPromptCacheMode, ResponsesReasoningEffort};

/// DeepSeek may learn a common prefix only after distinct suffix requests.
/// Run three state changes per arm; identical replay is only a control.
#[tokio::test]
#[ignore = "DeepSeek Flash live cache comparison: at most 10 paid synthetic requests"]
async fn compare_deepseek_flash_automatic_cache() {
    assert_eq!(std::env::var("KV_CACHE_LIVE").as_deref(), Ok("1"));
    let path = std::path::PathBuf::from(std::env::var("KV_CACHE_REPORT").expect("report required"));
    let (provider, profile) = live_configuration();
    assert_eq!(profile.base_url, "https://api.deepseek.com");
    assert_eq!(profile.model, "deepseek-flash");
    assert_eq!(profile.protocol, "responses");
    assert_eq!(
        profile.prompt_cache_mode,
        OpenAiPromptCacheMode::ProviderDefault
    );
    assert_eq!(
        profile.responses_reasoning_effort,
        ResponsesReasoningEffort::None
    );
    let nonce = RunId::new().to_string();
    let mut report = json!({"schema":"deepseek-automatic-cache/v1", "model":profile.model,
        "protocol":profile.protocol,"profile_digest":profile.digest(),
        "reasoning_effort":profile.responses_reasoning_effort.as_str(),
        "cache_mode":"provider_default","explicit_breakpoints_sent":false,
        "run_nonce":nonce,"started_unix":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        "max_requests":10,"max_output_tokens":profile.max_output_tokens,"gap_seconds":5,
        "transport_retries":false,"raw_requests_saved":false,"raw_answers_saved":false,
        "scope":"Synthetic inventory/focus check through production PromptAssembler and OpenAiProvider; not repository task quality",
        "billing_status":"No account bill was retrieved; usage is provider reported",
        "requests":[],"completed":false});
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .unwrap();
    write_report(&path, &report);
    let mut previous: [Option<WireRequestObservation>; 2] = [None, None];
    let mut boundaries = [None, None];
    let mut all_correct = true;
    let mut change_hits = [0usize; 2];
    for (phase, (round, operation)) in [
        (0, "cold"),
        (1, "change"),
        (2, "change"),
        (3, "change"),
        (3, "repeat"),
    ]
    .into_iter()
    .enumerate()
    {
        let pair = if phase % 2 == 0 {
            [PromptLayout::Legacy, PromptLayout::CurrentStateLast]
        } else {
            [PromptLayout::CurrentStateLast, PromptLayout::Legacy]
        };
        for layout in pair {
            let number = report["requests"].as_array().unwrap().len() + 1;
            if number > 1 {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            let slot = usize::from(layout == PromptLayout::CurrentStateLast);
            let request = input(layout, round, &format!("arm-{slot}-{nonce}"))
                .into_request(json!({"prompt_layout":layout}), CancellationToken::new());
            let boundary = request.prompt_reuse_boundary().unwrap();
            if let Some(previous) = &boundaries[slot] {
                assert_eq!(&boundary, previous);
            } else {
                boundaries[slot] = Some(boundary.clone());
            }
            let observer = CallObservation::default();
            let timing = TimingSink {
                start: Instant::now(),
                first_text_ms: AtomicU64::new(u64::MAX),
            };
            report["attempted_requests"] = json!(number);
            write_report(&path, &report);
            let result = provider
                .complete_stream_observed(request, &timing, &observer)
                .await;
            let sent = observer.requests.lock().unwrap();
            let mut row = json!({"sequence":number,"layout":layout,"round":round,"operation":operation,
                "boundary_hint":boundary,"elapsed_ms":timing.start.elapsed().as_millis() as u64,
                "wire_requests":&*sent,"response_observations":&*observer.responses.lock().unwrap(),
                "http_errors":&*observer.http_errors.lock().unwrap(),"dropped_observations":observer.dropped.load(Ordering::Relaxed)});
            if let Some(wire) = sent.first() {
                row["wire_difference"] = wire_difference(previous[slot].as_ref(), wire);
                previous[slot] = Some(wire.clone());
            }
            match result {
                Ok(output) => {
                    let answer = serde_json::from_str::<Value>(output.content.trim()).ok();
                    let correct =
                        answer.as_ref() == Some(&expected(round)) && output.tool_calls.is_empty();
                    all_correct &= correct;
                    if operation == "change"
                        && output
                            .usage
                            .cached_input_tokens
                            .is_some_and(|tokens| tokens > 0)
                    {
                        change_hits[slot] += 1;
                    }
                    row["usage"] = serde_json::to_value(&output.usage).unwrap();
                    row["answer_correct"] = json!(correct);
                    row["answer_sha256"] =
                        json!(ContentDigest::sha256_bytes(output.content.as_bytes()).to_string());
                    row["unexpected_tool_calls"] = json!(output.tool_calls.len());
                    let first = timing.first_text_ms.load(Ordering::Relaxed);
                    row["first_visible_text_ms"] = if first == u64::MAX {
                        Value::Null
                    } else {
                        json!(first)
                    };
                    println!(
                        "DEEPSEEK_CACHE sequence={number} layout={layout:?} operation={operation} input={:?} cached={:?} output={:?} correct={correct}",
                        output.usage.input_tokens,
                        output.usage.cached_input_tokens,
                        output.usage.output_tokens
                    );
                }
                Err(error) => {
                    row["transport_failure"] = boundary::transport_failure_summary(&error);
                    row["error_class"] = json!(match error {
                        AgentError::Transport { .. } | AgentError::TransportRetryAfter { .. } =>
                            "transport",
                        AgentError::ModelOutputLimit { .. } => "model_output_limit",
                        AgentError::ModelProtocol { .. } => "model_protocol",
                        _ => "other",
                    });
                    report["requests"].as_array_mut().unwrap().push(row);
                    write_report(&path, &report);
                    panic!("DeepSeek call failed; bounded report saved; no retries issued");
                }
            }
            report["requests"].as_array_mut().unwrap().push(row);
            write_report(&path, &report);
            assert_eq!(sent.len(), 1);
            assert_eq!(observer.dropped.load(Ordering::Relaxed), 0);
        }
    }
    report["completed"] = json!(true);
    report["all_answers_correct"] = json!(all_correct);
    report["change_requests_with_reported_reads"] =
        json!({"legacy":change_hits[0],"current_state_last":change_hits[1]});
    write_report(&path, &report);
    assert!(all_correct, "the synthetic focus/evidence check failed");
}
