use super::*;
use agent_platform_protocol::{
    JsonDecodeError, MAX_WORK_GOAL_BYTES, MAX_WORK_GOAL_CHARS, RequestId, decode_value,
    from_slice_bounded,
};
use serde_json::{Value, json};

fn work_request(operation: &str, text: &str) -> PlatformEnvelope<Value> {
    let message_id = MessageId::new();
    PlatformEnvelope {
        protocol: ProtocolIdentity {
            name: "focus-agent.platform".into(),
            version: agent_platform_protocol::ProtocolVersion { major: 1, minor: 0 },
            active_features: agent_platform_protocol::ActiveFeatures::default(),
            schema_digest: session_schema_digest(),
        },
        message_id,
        request_id: Some(RequestId::new()),
        kind: EnvelopeKind::Request,
        route: Route {
            namespace: "work".into(),
            operation: operation.into(),
        },
        work: None,
        causality: Causality::root(message_id),
        payload: if operation == "submit" {
            json!({"goal": text, "client_request_id": "long-text"})
        } else {
            json!({"instruction": text})
        },
    }
}

#[test]
fn legal_work_text_reaches_typed_validation_at_char_and_utf8_byte_caps() {
    // Exercise the separate character and replay-byte bounds, including a
    // maximum-size four-byte Unicode instruction, through the host decoder.
    for text in [
        "x".repeat(MAX_WORK_GOAL_CHARS),
        "汉".repeat(MAX_WORK_GOAL_BYTES / "汉".len()),
        "🦀".repeat(MAX_WORK_GOAL_BYTES / "🦀".len()),
    ] {
        for operation in ["submit", "steer"] {
            let request = work_request(operation, &text);
            if operation == "submit" {
                retyped::<WorkSubmitRequest>(&request)
                    .unwrap()
                    .payload
                    .validate()
                    .unwrap();
            } else {
                retyped::<WorkSteerRequest>(&request)
                    .unwrap()
                    .payload
                    .validate()
                    .unwrap();
            }
            let encoded = serde_json::to_vec(&request).unwrap();
            let mut stream = std::io::Cursor::new(Vec::new());
            write_frame(&mut stream, &encoded).unwrap();
            stream.set_position(0);
            let frame = read_frame(&mut stream).unwrap().unwrap();
            let decoded: PlatformEnvelope<Value> =
                from_slice_bounded(&frame, &decode_budget()).unwrap();
            assert_eq!(decoded.payload, request.payload, "{operation} text changed");
        }
    }
}

#[test]
fn oversized_work_text_decodes_but_is_refused_by_its_typed_contract() {
    for text in [
        "x".repeat(MAX_WORK_GOAL_CHARS + 1),
        "🦀".repeat(MAX_WORK_GOAL_BYTES / "🦀".len() + 1),
    ] {
        for operation in ["submit", "steer"] {
            let encoded = serde_json::to_vec(&work_request(operation, &text)).unwrap();
            assert!(encoded.len() < MAX_FRAME_BYTES as usize);
            let decoded: PlatformEnvelope<Value> =
                from_slice_bounded(&encoded, &decode_budget()).unwrap();
            let result = if operation == "submit" {
                retyped::<WorkSubmitRequest>(&decoded)
                    .unwrap()
                    .payload
                    .validate()
            } else {
                retyped::<WorkSteerRequest>(&decoded)
                    .unwrap()
                    .payload
                    .validate()
            };
            assert!(result.is_err(), "oversized {operation} was admitted");
        }
    }
}

#[test]
fn long_text_budget_keeps_structure_and_frame_guards() {
    // Actual hostile shapes, not just assertions that constants match.
    let deep = format!("{}0{}", "[".repeat(17), "]".repeat(17));
    assert!(matches!(
        decode_value(deep.as_bytes(), &decode_budget()),
        Err(JsonDecodeError::Depth { .. })
    ));
    assert!(matches!(
        decode_value(&serde_json::to_vec(&vec![0; 65]).unwrap(), &decode_budget()),
        Err(JsonDecodeError::ArrayLen { .. })
    ));
    let wide_object: serde_json::Map<String, Value> =
        (0..65).map(|i| (i.to_string(), Value::Null)).collect();
    assert!(matches!(
        decode_value(&serde_json::to_vec(&wide_object).unwrap(), &decode_budget()),
        Err(JsonDecodeError::ObjectKeys { .. })
    ));
    assert!(matches!(
        decode_value(
            &serde_json::to_vec(&vec![vec![0; 64]; 8]).unwrap(),
            &decode_budget()
        ),
        Err(JsonDecodeError::Nodes { .. })
    ));
    let mut oversized_header = std::io::Cursor::new((MAX_FRAME_BYTES + 1).to_le_bytes());
    assert_eq!(
        read_frame(&mut oversized_header).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert!(decode_value(br#"{"instruction":"unfinished"#, &decode_budget()).is_err());
}
