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
    /// Graceful stop: the service replies and exits 0.
    Shutdown,
}

/// One response per request. `ok: false` carries a human-readable error the
/// adapter maps back onto `AgentError::Context`.
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
    pub error: Option<String>,
}

impl ServiceResponse {
    pub fn ok(id: u64, value: Value) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            ok: true,
            value,
            error: None,
        }
    }

    pub fn error(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            ok: false,
            value: Value::Null,
            error: Some(error.into()),
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

        let error = ServiceResponse::error(8, "boom");
        let decoded: ServiceResponse =
            serde_json::from_str(&serde_json::to_string(&error).unwrap()).unwrap();
        assert!(!decoded.ok);
        assert_eq!(decoded.error.as_deref(), Some("boom"));
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
