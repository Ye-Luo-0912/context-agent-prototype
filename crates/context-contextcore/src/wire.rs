//! Wire protocol between the `ContextEngine` adapter and the context-service
//! process: one JSON request per line on stdin, one JSON response per line on
//! stdout. Both sides serialize the same `agent-contracts` types, so the
//! protocol carries no engine-specific vocabulary — exactly what keeps the
//! kernel contract unchanged when the service behind the socket is swapped
//! for a real ContextCore runtime.

use agent_contracts::{ContextBuildRequest, ContextIngress, ContextMaintenanceTrigger};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single request. `id` is echoed by the response for correlation and
/// debugging; the current adapter is strictly ping-pong (one in flight).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRequest {
    pub id: u64,
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
    BuildSnapshot {
        request: ContextBuildRequest,
    },
    Diagnostics,
    Inspect {
        limit: usize,
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
            ok: true,
            value,
            error: None,
        }
    }

    pub fn error(id: u64, error: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            value: Value::Null,
            error: Some(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_contracts::{ContextKind, ContextScope};

    #[test]
    fn request_response_round_trip() {
        let request = ServiceRequest {
            id: 7,
            op: ServiceOp::Ingest {
                ingress: ContextIngress::Pin {
                    content: "never touch generated files".into(),
                    kind: ContextKind::Constraint,
                },
            },
        };
        let line = serde_json::to_string(&request).unwrap();
        let decoded: ServiceRequest = serde_json::from_str(&line).unwrap();
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

        let error = ServiceResponse::error(8, "boom");
        let decoded: ServiceResponse =
            serde_json::from_str(&serde_json::to_string(&error).unwrap()).unwrap();
        assert!(!decoded.ok);
        assert_eq!(decoded.error.as_deref(), Some("boom"));
    }

    #[test]
    fn op_tag_is_snake_case_on_the_wire() {
        let request = ServiceRequest {
            id: 1,
            op: ServiceOp::BuildSnapshot {
                request: ContextBuildRequest {
                    system_prompt: "s".into(),
                    current_input: "i".into(),
                    budget_tokens: 100,
                },
            },
        };
        let line = serde_json::to_string(&request).unwrap();
        assert!(line.contains("\"op\":\"build_snapshot\""), "{line}");
    }
}
