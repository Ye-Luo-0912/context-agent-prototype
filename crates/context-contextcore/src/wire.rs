//! Wire protocol between the `ContextEngine` adapter and the context-service
//! process: one JSON request per line on stdin, one JSON response per line on
//! stdout. Both sides serialize the same `agent-contracts` types, so the
//! protocol carries no engine-specific vocabulary — exactly what keeps the
//! kernel contract unchanged when the service behind the socket is swapped
//! for a real ContextCore runtime.

use agent_contracts::{
    ContextConsumptionAck, ContextIngress, ContextItemId, ContextMaintenanceTrigger, ContextQuery,
    ContextSearchQuery, ScopeId, ScopeKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire protocol version. Both sides echo it; a mismatch fails the handshake
/// so a newer or older service is never misparsed. Defined by the shared
/// process host so every protocol over a JSON-lines pipe speaks one version.
pub use agent_process::PROTOCOL_VERSION;

/// W2 (V3): the capability a service advertises (in its handshake pong) for
/// the ATOMIC search op — `SearchExternalReport` returning hits + coverage +
/// observation in one value, plus real continuation forwarding. An adapter
/// facing a service that did not advertise it must treat the atomic facts as
/// `Unsupported` (never default them to `complete`).
pub const FEATURE_CONTEXT_SEARCH_REPORT: &str = "context-search-report.v1";

/// The feature names this service build advertises in its handshake pong.
/// The process host intersects them with what the adapter offered; a missing
/// entry is an explicit "old service" signal, not a silent fallback.
pub fn service_features() -> Vec<String> {
    vec![FEATURE_CONTEXT_SEARCH_REPORT.to_string()]
}

/// Default hard cap for one context-service JSON payload in either
/// direction. The trailing newline delimiter is not part of this count.
pub const DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES: usize = 1024;

/// A single request. `id` is echoed by the response for correlation and
/// debugging; the current adapter is strictly ping-pong (one in flight).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRequest {
    pub id: u64,
    /// Client protocol version, checked by the service.
    #[serde(default)]
    pub version: u32,
    #[serde(flatten)]
    pub op: ServiceOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServiceOp {
    /// Start-up handshake; the adapter refuses to continue unless this
    /// succeeds within the startup timeout.
    Ping,
    Ingest {
        ingress: ContextIngress,
    },
    Maintain {
        trigger: ContextMaintenanceTrigger,
    },
    Gc,
    /// Conservative Storage GC: permanently delete store entries whose
    /// semantic lifecycle ended and nothing references anymore (the only
    /// place information is deleted; the in-memory GC only externalizes).
    StorageGc,
    /// Startup reconcile: converge the on-disk blob directory with the
    /// external map after a crash or interrupted IO, so every formal blob
    /// has exactly one owner. Conservative — orphans are rebuilt into
    /// entries, damaged blobs are quarantined, never guessed away.
    ReconcileStore,
    Materialize {
        query: ContextQuery,
    },
    /// Commit access reinforcement for the exact final context frame used by
    /// one successful model operation.
    AcknowledgeConsumption {
        ack: ContextConsumptionAck,
    },
    OpenScope {
        kind: ScopeKind,
        parent: Option<ScopeId>,
    },
    CloseScope {
        scope_id: ScopeId,
    },
    Diagnostics,
    Inspect {
        limit: usize,
    },
    /// Bounded deterministic lookup over the external context map. The
    /// query carries its result limit; full content stays in the store.
    SearchExternal {
        query: ContextSearchQuery,
    },
    /// W2 (V3): the ATOMIC search — hits, typed coverage and observation of
    /// exactly one pass in a single response, with the continuation cursor
    /// riding inside `coverage.continuation`. `continuation: None` starts a
    /// fresh walk; `Some(token)` resumes the walk a previous pass issued.
    /// Superseded-by-capability: services that advertise
    /// [`FEATURE_CONTEXT_SEARCH_REPORT`] answer it with the serving engine's
    /// real facts; the legacy `SearchExternal` op remains for hit-only
    /// callers.
    SearchExternalReport {
        query: ContextSearchQuery,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continuation: Option<String>,
    },
    /// Read one external map entry's metadata without touching the store.
    InspectExternal {
        item_id: ContextItemId,
    },
    /// Read one externalized item's full content from the store without
    /// reactivating it into the working set.
    FetchExternal {
        item_id: ContextItemId,
    },
    Checkpoint,
    Restore {
        data: Value,
    },
    /// EXEC-9 (R3-01): the item ids a stored context checkpoint references
    /// as external blobs — the strong recovery roots reconcile/Storage GC
    /// must not delete while the checkpoint is retained and restorable.
    /// The payload is the engine's own serialized checkpoint state; parsing
    /// is the engine's real, versioned implementation behind this op, so a
    /// service answers exactly like the in-process engine.
    CheckpointRecoveryItemIds {
        checkpoint: Value,
    },
    /// EXEC-9 (R3-01): conservative Storage GC under the shared
    /// retained-root invariant — `roots` are the item ids retained
    /// checkpoints still reference and must never be deleted;
    /// `roots_complete == false` defers every deletion (an incomplete root
    /// set cannot prove "nothing retained").
    StorageGcProtecting {
        roots: Vec<ContextItemId>,
        roots_complete: bool,
    },
    /// EXEC-9 (R3-01): startup reconcile under the same retained-root
    /// invariant — the stale-duplicate deletion branch defers while the
    /// roots are incomplete or still referenced.
    ReconcileStoreProtecting {
        roots: Vec<ContextItemId>,
        roots_complete: bool,
    },
    /// Graceful stop: the service replies and exits 0.
    Shutdown,
}

/// One response per request. `ok: false` carries a bounded typed error the
/// adapter maps back onto `AgentError::Context`; the category travels the
/// wire so in-process and process-boundary callers classify the failure
/// identically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceResponse {
    pub id: u64,
    /// Service protocol version, echoed on every response so the client can
    /// detect a mismatch from the very first frame.
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub value: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ServiceErrorEnvelope>,
    /// W2 (V3): capability advertisement, set ONLY on the handshake pong —
    /// the shared process host reads a top-level `features` array from the
    /// handshake response and intersects it with what the client offered.
    /// Absent on every other response and on pre-V3 pongs (an old service),
    /// which is exactly the explicit "capability not present" signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
}

/// Structural error category shared across the process boundary. Classifying
/// at the wire shape — not inside a diagnostic string — keeps the in-process
/// handler and the adapter on the same retry/terminal policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceErrorCategory {
    /// The sidecar failed to read one request frame (delimiter, UTF-8, or a
    /// frame that extends past EOF). The byte stream is no longer trusted.
    Framing,
    /// The request violated the wire contract after framing (JSON decode
    /// budget, body shape, protocol version). The stream is still framed.
    Protocol,
    /// The engine operation itself failed.
    Engine,
    /// The engine's persistence/store layer failed.
    Store,
    /// The sidecar failed to write one response frame.
    Io,
}

/// Bounded diagnostic budget for `message`: the envelope must always fit a
/// minimal frame cap (UTF-8 worst case is four bytes per character plus the
/// envelope's own JSON structure).
pub const MAX_SERVICE_ERROR_CHARS: usize = 192;

/// Bounded typed error carried by [`ServiceResponse`]. `category` and
/// `retryable` are contract; `message` is human-readable diagnostics and is
/// truncated at construction so an oversized engine error cannot blow the
/// frame bound.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceErrorEnvelope {
    pub category: ServiceErrorCategory,
    pub retryable: bool,
    pub message: String,
}

impl ServiceErrorEnvelope {
    pub fn new(category: ServiceErrorCategory, retryable: bool, message: impl AsRef<str>) -> Self {
        let bounded: String = message
            .as_ref()
            .chars()
            .take(MAX_SERVICE_ERROR_CHARS)
            .collect();
        Self {
            category,
            retryable,
            message: bounded,
        }
    }
}

impl ServiceResponse {
    pub fn ok(id: u64, value: Value) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            ok: true,
            value,
            error: None,
            features: None,
        }
    }

    pub fn error(
        id: u64,
        category: ServiceErrorCategory,
        retryable: bool,
        message: impl AsRef<str>,
    ) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            ok: false,
            value: Value::Null,
            error: Some(ServiceErrorEnvelope::new(category, retryable, message)),
            features: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{
        ContextItemId, ContextKind, ContextScope, ContextSearchQuery, OperationId, TurnId,
    };

    #[test]
    fn request_response_round_trip() {
        let request = ServiceRequest {
            id: 7,
            version: PROTOCOL_VERSION,
            op: ServiceOp::Ingest {
                ingress: ContextIngress::Pin {
                    content: "never touch generated files".into(),
                    kind: ContextKind::Constraint,
                },
            },
        };
        let line = serde_json::to_string(&request).unwrap();
        let decoded: ServiceRequest = serde_json::from_str(&line).unwrap();
        assert_eq!(decoded.version, PROTOCOL_VERSION);
        match decoded.op {
            ServiceOp::Ingest {
                ingress: ContextIngress::Pin { content, kind },
            } => {
                assert_eq!(content, "never touch generated files");
                assert_eq!(kind, ContextKind::Constraint);
            }
            other => panic!("unexpected op: {other:?}"),
        }

        let response = ServiceResponse::ok(7, serde_json::to_value(ContextScope::Pinned).unwrap());
        let line = serde_json::to_string(&response).unwrap();
        let decoded: ServiceResponse = serde_json::from_str(&line).unwrap();
        assert!(decoded.ok);
        assert_eq!(decoded.id, 7);
        assert_eq!(decoded.version, PROTOCOL_VERSION);

        let error = ServiceResponse::error(8, ServiceErrorCategory::Engine, false, "boom");
        let decoded: ServiceResponse =
            serde_json::from_str(&serde_json::to_string(&error).unwrap()).unwrap();
        assert!(!decoded.ok);
        let envelope = decoded.error.expect("a typed error envelope");
        assert_eq!(envelope.category, ServiceErrorCategory::Engine);
        assert!(!envelope.retryable);
        assert_eq!(envelope.message, "boom");
    }

    #[test]
    fn error_message_is_bounded_at_construction() {
        let envelope = ServiceErrorEnvelope::new(
            ServiceErrorCategory::Engine,
            true,
            "x".repeat(MAX_SERVICE_ERROR_CHARS * 4),
        );
        assert_eq!(envelope.message.chars().count(), MAX_SERVICE_ERROR_CHARS);
        let wire = serde_json::to_value(&envelope).unwrap();
        assert!(
            wire.to_string().len() < MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES,
            "the bounded envelope always fits a minimal frame"
        );
    }

    #[test]
    fn error_category_is_snake_case_on_the_wire() {
        let wire = serde_json::to_value(ServiceErrorEnvelope::new(
            ServiceErrorCategory::Protocol,
            false,
            "bad version",
        ))
        .unwrap();
        assert_eq!(wire["category"], serde_json::json!("protocol"));
    }

    #[test]
    fn version_mismatch_is_observable_on_the_wire() {
        // A response from a future service carries a different version; the
        // adapter rejects it instead of misparsing the payload.
        let mut response = ServiceResponse::ok(1, serde_json::Value::Null);
        response.version = PROTOCOL_VERSION + 1;
        let decoded: ServiceResponse =
            serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
        assert_ne!(decoded.version, PROTOCOL_VERSION);
    }

    #[test]
    fn op_tag_is_snake_case_on_the_wire() {
        let request = ServiceRequest {
            id: 1,
            version: PROTOCOL_VERSION,
            op: ServiceOp::Materialize {
                query: ContextQuery {
                    current_input: "i".into(),
                    budget_tokens: 100,
                    hints: Default::default(),
                },
            },
        };
        let line = serde_json::to_string(&request).unwrap();
        assert!(line.contains("\"op\":\"materialize\""), "{line}");
    }

    #[test]
    fn external_recall_ops_round_trip_with_typed_ids_and_bounds() {
        let item_id = ContextItemId::new();
        let ops = [
            ServiceOp::SearchExternal {
                query: ContextSearchQuery::new("AuthService", 7),
            },
            ServiceOp::InspectExternal { item_id },
            ServiceOp::FetchExternal { item_id },
        ];

        for (index, op) in ops.into_iter().enumerate() {
            let request = ServiceRequest {
                id: index as u64,
                version: PROTOCOL_VERSION,
                op,
            };
            let encoded = serde_json::to_string(&request).unwrap();
            let decoded: ServiceRequest = serde_json::from_str(&encoded).unwrap();
            match decoded.op {
                ServiceOp::SearchExternal { query } => {
                    assert_eq!(query.query, "AuthService");
                    assert_eq!(query.limit, 7);
                }
                ServiceOp::InspectExternal { item_id: decoded }
                | ServiceOp::FetchExternal { item_id: decoded } => {
                    assert_eq!(decoded, item_id);
                }
                other => panic!("unexpected recall op: {other:?}"),
            }
        }
    }

    #[test]
    fn search_report_ops_round_trip_with_and_without_a_token() {
        // The atomic op is a distinct wire tag; fresh (None) and resumed
        // (Some) passes both encode and decode losslessly, and the fresh
        // form omits the field entirely (an old-side decode budget stays
        // byte-identical to the pre-V3 shape).
        for (index, op) in [
            ServiceOp::SearchExternalReport {
                query: ContextSearchQuery::new("AuthService", 9),
                continuation: None,
            },
            ServiceOp::SearchExternalReport {
                query: ContextSearchQuery::new("AuthService", 9),
                continuation: Some("cold-window-e3-n17".into()),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let request = ServiceRequest {
                id: index as u64,
                version: PROTOCOL_VERSION,
                op,
            };
            let encoded = serde_json::to_string(&request).unwrap();
            assert!(
                encoded.contains("\"op\":\"search_external_report\""),
                "{encoded}"
            );
            let decoded: ServiceRequest = serde_json::from_str(&encoded).unwrap();
            match decoded.op {
                ServiceOp::SearchExternalReport {
                    query,
                    continuation,
                } => {
                    assert_eq!(query.query, "AuthService");
                    assert_eq!(query.limit, 9);
                    match index {
                        0 => assert_eq!(continuation, None),
                        _ => assert_eq!(
                            continuation.as_deref(),
                            Some("cold-window-e3-n17"),
                            "the continuation token crosses the wire verbatim"
                        ),
                    }
                }
                other => panic!("unexpected op: {other:?}"),
            }
        }
        let fresh = serde_json::to_string(&ServiceRequest {
            id: 0,
            version: PROTOCOL_VERSION,
            op: ServiceOp::SearchExternalReport {
                query: ContextSearchQuery::new("x", 1),
                continuation: None,
            },
        })
        .unwrap();
        assert!(
            !fresh.contains("continuation"),
            "the fresh form omits the field: {fresh}"
        );
    }

    #[test]
    fn handshake_pong_carries_the_capability_advertisement() {
        // The feature table rides as a top-level frame field (where the
        // shared process host's handshake reads it), not inside `value`.
        let mut response = ServiceResponse::ok(1, serde_json::Value::String("pong".into()));
        response.features = Some(service_features());
        let encoded = serde_json::to_value(&response).unwrap();
        assert_eq!(
            encoded["features"],
            serde_json::json!([FEATURE_CONTEXT_SEARCH_REPORT]),
            "the pong frame names the atomic-search capability"
        );
        let decoded: ServiceResponse =
            serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
        assert_eq!(decoded.features, Some(service_features()));

        // A pre-V3 pong (no field) decodes as an explicit absence, and so
        // does every non-pong response.
        let legacy = serde_json::from_str::<ServiceResponse>(
            &serde_json::to_string(&ServiceResponse::ok(
                2,
                serde_json::Value::String("pong".into()),
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(legacy.features, None, "old pongs advertise nothing");
    }

    #[test]
    fn consumption_ack_round_trips_with_operation_and_preview_identity() {
        let item_id = ContextItemId::new();
        let external_id = ContextItemId::new();
        let ack = ContextConsumptionAck {
            turn_id: TurnId::new(),
            operation_id: OperationId::new(),
            model_round: 3,
            materialization_id: 17,
            item_ids: vec![item_id],
            external_item_ids: vec![external_id],
            foreground_item_ids: Vec::new(),
        };
        let request = ServiceRequest {
            id: 19,
            version: PROTOCOL_VERSION,
            op: ServiceOp::AcknowledgeConsumption { ack: ack.clone() },
        };

        let encoded = serde_json::to_string(&request).unwrap();
        assert!(
            encoded.contains("\"op\":\"acknowledge_consumption\""),
            "{encoded}"
        );
        let decoded: ServiceRequest = serde_json::from_str(&encoded).unwrap();
        match decoded.op {
            ServiceOp::AcknowledgeConsumption { ack: decoded } => assert_eq!(decoded, ack),
            other => panic!("unexpected op: {other:?}"),
        }
    }
}
