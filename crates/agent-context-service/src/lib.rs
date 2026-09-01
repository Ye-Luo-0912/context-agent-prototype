//! The context-service protocol handler: one `ServiceOp` in, one `Value`
//! out, executed against a real in-process `ContextEngine`.
//!
//! The binary wraps this in the stdio JSON-lines loop (one request per
//! line on stdin, one response per line on stdout, see
//! `context-contextcore::wire`). Exposing the handler as a lib lets the
//! adapter's integration tests depend on this crate — which also forces
//! cargo to rebuild the binary whenever the wire protocol changes, so the
//! process-boundary parity tests always exercise the current protocol.
//! The session loop is here too so framing failures can be tested without
//! relying on OS pipe packetization.

use std::path::PathBuf;
use std::sync::Arc;

use agent_contracts::{AgentError, ContextEngine};
use agent_platform_protocol::{JsonDecodeBudget, from_slice_bounded};
use agent_process::{FrameErrorKind, encode_frame, read_frame};
use context_baselines::{AppendOnlyEngine, RollingSummaryEngine};
use context_contextcore::{
    PROTOCOL_VERSION, ServiceErrorCategory, ServiceOp, ServiceRequest, ServiceResponse,
};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncWrite, AsyncWriteExt};

/// The engine behind the service, chosen with `--engine`. `dynamic` is the
/// real context engine; the two baselines are kept so the wire protocol can
/// be exercised against engines without a GC/store at all. `store_dir`
/// (from `--store-dir`) pins the context store under the caller-provided
/// state dir; `None` falls back to the engine's temp-dir default — never a
/// CWD-relative path.
pub fn build_engine(engine: &str, store_dir: Option<PathBuf>) -> Arc<dyn ContextEngine> {
    match engine {
        "dynamic" => Arc::new(SimpleContextEngine::new(SimpleContextConfig {
            context_store_dir: store_dir,
            ..SimpleContextConfig::default()
        })),
        "append" => Arc::new(AppendOnlyEngine::new()),
        "rolling" => Arc::new(RollingSummaryEngine::with_config(
            context_baselines::RollingConfig::default(),
        )),
        other => {
            eprintln!("unknown engine: {other}");
            std::process::exit(2);
        }
    }
}

/// Execute one protocol operation against the engine. The response value is
/// serialized by the caller; an error becomes `ServiceResponse::error`.
pub async fn handle(op: ServiceOp, engine: &dyn ContextEngine) -> Result<Value, AgentError> {
    match op {
        ServiceOp::Ping => Ok(Value::String("pong".into())),
        ServiceOp::Ingest { ingress } => {
            engine.ingest(ingress).await?;
            Ok(Value::Null)
        }
        ServiceOp::Maintain { trigger } => {
            let report = engine.maintain(trigger).await?;
            serde_json::to_value(report).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::Gc => {
            let report = engine.gc().await?;
            serde_json::to_value(report).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::StorageGc => {
            let report = engine.storage_gc().await?;
            serde_json::to_value(report).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::ReconcileStore => {
            let report = engine.reconcile_store().await?;
            serde_json::to_value(report).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::Materialize { query } => {
            let materialized = engine.materialize(query).await?;
            serde_json::to_value(materialized).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::AcknowledgeConsumption { ack } => {
            engine.acknowledge_consumption(ack).await?;
            Ok(Value::Null)
        }
        ServiceOp::OpenScope { kind, parent } => {
            let scope_id = engine.open_scope(kind, parent).await?;
            serde_json::to_value(scope_id).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::CloseScope { scope_id } => {
            let transitions = engine.close_scope(scope_id).await?;
            serde_json::to_value(transitions).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::Diagnostics => {
            let diagnostics = engine.diagnostics().await?;
            serde_json::to_value(diagnostics).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::Inspect { limit } => {
            let items = engine.inspect(limit).await?;
            serde_json::to_value(items).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::SearchExternal { query } => {
            let entries = engine.search_external(query).await?;
            serde_json::to_value(entries).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::InspectExternal { item_id } => {
            let entry = engine.inspect_external(item_id).await?;
            serde_json::to_value(entry).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::FetchExternal { item_id } => {
            let item = engine.fetch_external(item_id).await?;
            serde_json::to_value(item).map_err(|e| AgentError::Context(e.to_string()))
        }
        ServiceOp::Checkpoint => engine.checkpoint().await,
        ServiceOp::Restore { data } => {
            engine.restore(data).await?;
            Ok(Value::Null)
        }
        ServiceOp::Shutdown => Ok(Value::Null),
    }
}

/// How one session ended. `Clean` is a normal disconnect (clean EOF or a
/// graceful `Shutdown` request); the sidecar exits 0. `ProtocolViolation`
/// is a terminal protocol failure that already received an error frame —
/// the caller must exit non-zero so a supervisor never mistakes a dead
/// protocol run for a healthy service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    Clean,
    ProtocolViolation,
}

/// Serve one single-in-flight context protocol session.
///
/// Clean EOF is a normal disconnect. Every other framing or protocol
/// violation receives at most one bounded error response and then closes
/// the session as `ProtocolViolation`. Responses that exceed the configured
/// cap are replaced by a bounded error response before any bytes are
/// written, after which the session also closes because the caller did not
/// receive the operation's actual result. A write failure returns
/// `Err(io::Error)`; the stream is gone, so the session is terminal either
/// way.
pub async fn serve_session<R, W>(
    reader: &mut R,
    writer: &mut W,
    engine: &dyn ContextEngine,
    max_frame_bytes: usize,
) -> std::io::Result<SessionEnd>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let frame = match read_frame(reader, max_frame_bytes).await {
            Ok(frame) => frame,
            Err(error) if error.kind == FrameErrorKind::Eof => return Ok(SessionEnd::Clean),
            Err(error) => {
                let response = ServiceResponse::error(
                    0,
                    ServiceErrorCategory::Framing,
                    false,
                    format!("bad request: {error}"),
                );
                let _ = write_response(writer, &response, max_frame_bytes).await?;
                return Ok(SessionEnd::ProtocolViolation);
            }
        };

        // 解析期执行数据面 JSON 预算，再投影到 ServiceRequest。
        let budget = JsonDecodeBudget::for_frame_bytes(max_frame_bytes);
        let request: ServiceRequest = match from_slice_bounded(&frame, &budget) {
            Ok(request) => request,
            Err(error) => {
                let response = ServiceResponse::error(
                    0,
                    ServiceErrorCategory::Protocol,
                    false,
                    format!("bad request: {error}"),
                );
                let _ = write_response(writer, &response, max_frame_bytes).await?;
                return Ok(SessionEnd::ProtocolViolation);
            }
        };
        let id = request.id;
        if request.version != PROTOCOL_VERSION {
            let response = ServiceResponse::error(
                id,
                ServiceErrorCategory::Protocol,
                false,
                format!(
                    "protocol version mismatch: client {}, service {PROTOCOL_VERSION}",
                    request.version
                ),
            );
            let _ = write_response(writer, &response, max_frame_bytes).await?;
            return Ok(SessionEnd::ProtocolViolation);
        }

        let shutdown = matches!(request.op, ServiceOp::Shutdown);
        let result = handle(request.op, engine).await;
        let response = match result {
            Ok(value) => ServiceResponse::ok(id, value),
            Err(error) => {
                ServiceResponse::error(id, ServiceErrorCategory::Engine, false, error.to_string())
            }
        };
        let disposition = write_response(writer, &response, max_frame_bytes).await?;
        if shutdown {
            return Ok(SessionEnd::Clean);
        }
        if disposition != WriteDisposition::Original {
            return Ok(SessionEnd::ProtocolViolation);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteDisposition {
    Original,
    Replacement,
    NotWritten,
}

/// Serialize and write one complete response frame. An oversized response is
/// never partially written: it is replaced by a small error frame when that
/// frame fits, otherwise nothing is written. The caller closes the session
/// after either non-original outcome.
async fn write_response<W>(
    writer: &mut W,
    response: &ServiceResponse,
    max_frame_bytes: usize,
) -> std::io::Result<WriteDisposition>
where
    W: AsyncWrite + Unpin,
{
    let value = serde_json::to_value(response).map_err(invalid_data)?;
    let (line, disposition) = match encode_frame(&value, max_frame_bytes) {
        Ok(line) => (line, WriteDisposition::Original),
        Err(_) => {
            let replacement = ServiceResponse::error(
                response.id,
                ServiceErrorCategory::Protocol,
                false,
                "response exceeded frame bound",
            );
            let value = serde_json::to_value(replacement).map_err(invalid_data)?;
            match encode_frame(&value, max_frame_bytes) {
                Ok(line) => (line, WriteDisposition::Replacement),
                Err(_) => return Ok(WriteDisposition::NotWritten),
            }
        }
    };
    writer.write_all(&line).await?;
    writer.flush().await?;
    Ok(disposition)
}

fn invalid_data(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    async fn run_session(input: &[u8], max_frame_bytes: usize) -> (Vec<u8>, SessionEnd) {
        let engine = AppendOnlyEngine::new();
        let mut reader = BufReader::new(input);
        let mut output = Vec::new();
        let end = serve_session(&mut reader, &mut output, &engine, max_frame_bytes)
            .await
            .unwrap();
        (output, end)
    }

    fn error_response(output: &[u8]) -> ServiceResponse {
        assert_eq!(output.iter().filter(|byte| **byte == b'\n').count(), 1);
        serde_json::from_slice(output).expect("one valid error response")
    }

    #[tokio::test]
    async fn clean_eof_is_a_clean_session_end() {
        let (output, end) = run_session(b"", 1024).await;
        assert!(output.is_empty());
        assert_eq!(end, SessionEnd::Clean);
    }

    #[tokio::test]
    async fn graceful_shutdown_is_a_clean_session_end() {
        let (output, end) =
            run_session(b"{\"id\":1,\"version\":1,\"op\":\"shutdown\"}\n", 1024).await;
        let response = error_response(&output);
        assert!(response.ok);
        assert_eq!(end, SessionEnd::Clean);
    }

    #[tokio::test]
    async fn partial_eof_fails_closed() {
        let (output, end) = run_session(b"{\"id\":1", 1024).await;
        let response = error_response(&output);
        assert!(!response.ok);
        let envelope = response.error.expect("a typed error envelope");
        assert_eq!(envelope.category, ServiceErrorCategory::Framing);
        assert!(envelope.message.contains("mid-frame"));
        assert_eq!(end, SessionEnd::ProtocolViolation);
    }

    #[tokio::test]
    async fn buffered_requests_are_processed_as_distinct_frames() {
        let input = concat!(
            "{\"id\":1,\"version\":1,\"op\":\"ping\"}\n",
            "{\"id\":2,\"version\":1,\"op\":\"ping\"}\n"
        );
        let (output, end) = run_session(input.as_bytes(), 1024).await;
        let responses: Vec<ServiceResponse> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 2);
        assert!(responses.iter().all(|response| response.ok));
        assert_eq!(end, SessionEnd::Clean);
    }

    #[tokio::test]
    async fn malformed_utf8_fails_closed() {
        let (output, end) = run_session(&[0xff, b'\n'], 1024).await;
        let response = error_response(&output);
        assert!(!response.ok);
        let envelope = response.error.expect("a typed error envelope");
        assert_eq!(
            envelope.category,
            ServiceErrorCategory::Protocol,
            "undecodable bytes fail at the JSON decode layer, not the frame reader"
        );
        assert!(envelope.message.contains("bad request"));
        assert_eq!(end, SessionEnd::ProtocolViolation);
    }

    #[tokio::test]
    async fn malformed_json_fails_closed() {
        let (output, end) = run_session(b"{not-json}\n", 1024).await;
        let response = error_response(&output);
        assert!(!response.ok);
        let envelope = response.error.expect("a typed error envelope");
        assert_eq!(envelope.category, ServiceErrorCategory::Protocol);
        assert!(envelope.message.contains("bad request"));
        assert_eq!(end, SessionEnd::ProtocolViolation);
    }

    #[tokio::test]
    async fn decoded_json_node_budget_fails_closed() {
        let mut input = String::from("{\"id\":1,\"version\":1,\"op\":\"ping\",\"pad\":[");
        for index in 0..200 {
            if index > 0 {
                input.push(',');
            }
            input.push_str("{}");
        }
        input.push_str("]}\n");
        assert!(
            input.len() < 1024,
            "the bomb must still fit the encoded frame cap"
        );
        let (output, end) = run_session(input.as_bytes(), 1024).await;
        let response = error_response(&output);
        assert!(!response.ok);
        let envelope = response.error.expect("a typed error envelope");
        assert_eq!(envelope.category, ServiceErrorCategory::Protocol);
        assert!(
            envelope.message.contains("json decode budget"),
            "decoded node budget must fail closed before ServiceRequest projection"
        );
        assert_eq!(end, SessionEnd::ProtocolViolation);
    }

    #[tokio::test]
    async fn wrong_protocol_version_fails_closed() {
        let (output, end) = run_session(b"{\"id\":7,\"version\":99,\"op\":\"ping\"}\n", 1024).await;
        let response = error_response(&output);
        assert_eq!(response.id, 7);
        assert!(!response.ok);
        let envelope = response.error.expect("a typed error envelope");
        assert_eq!(envelope.category, ServiceErrorCategory::Protocol);
        assert!(envelope.message.contains("version mismatch"));
        assert_eq!(end, SessionEnd::ProtocolViolation);
    }

    #[tokio::test]
    async fn engine_failure_is_classified_and_the_session_continues() {
        let input = concat!(
            "{\"id\":1,\"version\":1,\"op\":\"restore\",\"data\":null}\n",
            "{\"id\":2,\"version\":1,\"op\":\"ping\"}\n"
        );
        let (output, end) = run_session(input.as_bytes(), 1024).await;
        let lines: Vec<&[u8]> = output
            .split(|byte| *byte == b'\n')
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(lines.len(), 2);

        let failed: ServiceResponse = serde_json::from_slice(lines[0]).unwrap();
        assert!(!failed.ok);
        let envelope = failed.error.expect("a typed error envelope");
        assert_eq!(envelope.category, ServiceErrorCategory::Engine);
        assert!(
            !envelope.retryable,
            "an engine failure is not blindly retried"
        );

        let follow_up: ServiceResponse = serde_json::from_slice(lines[1]).unwrap();
        assert!(
            follow_up.ok,
            "an engine error is one op failing, not the session"
        );

        assert_eq!(end, SessionEnd::Clean);
    }

    #[tokio::test]
    async fn oversized_response_is_replaced_before_writing() {
        let response = ServiceResponse::ok(42, Value::String("x".repeat(4096)));
        let mut output = Vec::new();
        let disposition = write_response(&mut output, &response, 256).await.unwrap();
        assert_eq!(disposition, WriteDisposition::Replacement);
        assert!(output.len() <= 257, "delimiter is outside the payload cap");
        let replacement = error_response(&output);
        assert_eq!(replacement.id, 42);
        assert!(!replacement.ok);
        let envelope = replacement.error.expect("a typed error envelope");
        assert_eq!(envelope.category, ServiceErrorCategory::Protocol);
        assert!(envelope.message.contains("frame bound"));
    }

    #[tokio::test]
    async fn too_small_cap_closes_without_writing_or_panicking() {
        let response = ServiceResponse::ok(42, Value::String("x".repeat(4096)));
        let mut output = Vec::new();
        let disposition = write_response(&mut output, &response, 8).await.unwrap();
        assert_eq!(disposition, WriteDisposition::NotWritten);
        assert!(output.is_empty());
    }
}
