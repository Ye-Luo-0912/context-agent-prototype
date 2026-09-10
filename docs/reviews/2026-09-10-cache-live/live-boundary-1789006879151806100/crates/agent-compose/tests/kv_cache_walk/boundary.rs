use super::*;
use provider_openai::OpenAiPromptCacheMode;

#[test]
fn changed_focus_fixture_preserves_the_bound_evidence_prefix() {
    let request = |round| {
        input(PromptLayout::CurrentStateLast, round, "boundary-fixture")
            .into_request(json!({}), Default::default())
    };
    assert_eq!(
        request(0).prompt_reuse_boundary(),
        request(1).prompt_reuse_boundary()
    );
    assert_ne!(
        encoded_messages(&input(
            PromptLayout::CurrentStateLast,
            0,
            "boundary-fixture"
        )),
        encoded_messages(&input(
            PromptLayout::CurrentStateLast,
            1,
            "boundary-fixture"
        ))
    );
}

/// Capability probe and iteration check. One new control variable: explicit
/// boundary mapping. Acceptance of JSON alone is not proof it was honored.
#[tokio::test]
#[ignore = "live provider: at most four paid calls; explicit opt-in, synthetic input only"]
async fn probe_explicit_reuse_boundary() {
    assert_eq!(std::env::var("KV_CACHE_LIVE").as_deref(), Ok("1"));
    let path =
        std::path::PathBuf::from(std::env::var("KV_CACHE_REPORT").expect("report path required"));
    let (provider, profile) = live_configuration();
    assert_eq!(
        profile.prompt_cache_mode,
        OpenAiPromptCacheMode::ResponsesExplicit
    );
    let nonce = RunId::new().to_string();
    let mut report = json!({"schema":"kv-boundary-live/v1", "run_nonce":nonce,
        "started_unix":SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        "model":profile.model,"protocol":profile.protocol,"profile_digest":profile.digest(),
        "prompt_cache_mode":profile.prompt_cache_mode.as_str(), "max_requests":4,
        "max_output_tokens":profile.max_output_tokens,"transport_retries":false,"gap_seconds":5,
        "raw_requests_saved":false,"cost_status":"UNKNOWN: gateway pricing and write semantics not verified",
        "scope":"Synthetic focus/evidence check; field acceptance does not establish that the gateway honors breakpoints",
        "requests":[],"completed":false});
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("new report path required");
    write_report(&path, &report);
    let mut previous = None;
    let mut expected_boundary = None;
    let mut all_correct = true;
    let mut changed_state_hits = 0;
    for (index, (round, operation)) in [(0, "cold"), (0, "repeat"), (1, "change"), (2, "change")]
        .into_iter()
        .enumerate()
    {
        if index > 0 {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        let request = input(PromptLayout::CurrentStateLast, round, &nonce).into_request(
            json!({"prompt_layout":PromptLayout::CurrentStateLast}),
            CancellationToken::new(),
        );
        let boundary = request.prompt_reuse_boundary().unwrap();
        if let Some(expected) = &expected_boundary {
            assert_eq!(&boundary, expected);
        } else {
            expected_boundary = Some(boundary.clone());
        }
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
        let sent = observer.requests.lock().unwrap();
        let mut row = json!({"sequence":index + 1,"round":round,"operation":operation,
            "boundary":boundary,"elapsed_ms":timing.start.elapsed().as_millis() as u64,
            "wire_requests":&*sent,"response_observations":&*observer.responses.lock().unwrap(),
            "dropped_observations":observer.dropped.load(Ordering::Relaxed)});
        if let Some(wire) = sent.first() {
            row["wire_difference"] = wire_difference(previous.as_ref(), wire);
            previous = Some(wire.clone());
        }
        match result {
            Ok(output) => {
                let answer: Option<Value> = serde_json::from_str(output.content.trim()).ok();
                let correct =
                    answer.as_ref() == Some(&expected(round)) && output.tool_calls.is_empty();
                all_correct &= correct;
                if operation == "change"
                    && output
                        .usage
                        .cached_input_tokens
                        .is_some_and(|tokens| tokens > 0)
                {
                    changed_state_hits += 1;
                }
                row["usage"] = serde_json::to_value(&output.usage).unwrap();
                row["answer_correct"] = json!(correct);
                row["answer_sha256"] =
                    json!(ContentDigest::sha256_bytes(output.content.as_bytes()).to_string());
                row["unexpected_tool_calls"] = json!(output.tool_calls.len());
                println!(
                    "KV_BOUNDARY sequence={} operation={operation} input={:?} cached={:?} output={:?} correct={correct}",
                    index + 1,
                    output.usage.input_tokens,
                    output.usage.cached_input_tokens,
                    output.usage.output_tokens
                );
            }
            Err(error) => {
                row["error_class"] = json!(match error {
                    AgentError::Cancelled => "cancelled",
                    AgentError::Transport { .. } | AgentError::TransportRetryAfter { .. } =>
                        "transport",
                    AgentError::ModelProtocol { .. } => "model_protocol",
                    _ => "other",
                });
                report["requests"].as_array_mut().unwrap().push(row);
                write_report(&path, &report);
                panic!("boundary probe failed; sanitized report saved; no retries issued");
            }
        }
        report["requests"].as_array_mut().unwrap().push(row);
        write_report(&path, &report);
        assert_eq!(sent.len(), 1);
        assert_eq!(observer.dropped.load(Ordering::Relaxed), 0);
    }
    report["completed"] = json!(true);
    report["all_answers_correct"] = json!(all_correct);
    report["changed_state_requests_with_reported_reads"] = json!(changed_state_hits);
    report["iteration_cache_reads_observed"] = json!(all_correct && changed_state_hits == 2);
    write_report(&path, &report);
    assert!(
        all_correct,
        "focus/evidence regression; see sanitized report"
    );
    // Cache acceptance is recorded separately: successful calls are never
    // mislabeled as a verified cost reduction or a proven upstream identity.
}
