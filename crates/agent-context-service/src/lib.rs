//! The context-service protocol handler: one `ServiceOp` in, one `Value`
//! out, executed against a real in-process `ContextEngine`.
//!
//! The binary wraps this in the stdio JSON-lines loop (one request per
//! line on stdin, one response per line on stdout, see
//! `context-contextcore::wire`). Exposing the handler as a lib lets the
//! adapter's integration tests depend on this crate — which also forces
//! cargo to rebuild the binary whenever the wire protocol changes, so the
//! process-boundary parity tests always exercise the current protocol.

use std::path::PathBuf;
use std::sync::Arc;

use agent_contracts::{AgentError, ContextEngine};
use context_baselines::{AppendOnlyEngine, RollingSummaryEngine};
use context_contextcore::ServiceOp;
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::Value;

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
