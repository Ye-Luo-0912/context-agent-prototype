//! Standalone context service.
//!
//! Reads JSON requests from stdin and writes JSON responses to stdout (one
//! line each, see `context-contextcore::wire`). It runs a real in-process
//! `ContextEngine` chosen with `--engine`; the adapter crate is the client.
//! A future real ContextCore runtime only has to speak the same protocol —
//! nothing on the agent side changes.

use std::sync::Arc;

use agent_contracts::{AgentError, ContextEngine};
use context_baselines::{AppendOnlyEngine, RollingSummaryEngine};
use context_contextcore::{PROTOCOL_VERSION, ServiceOp, ServiceRequest, ServiceResponse};
use context_simple::{SimpleContextConfig, SimpleContextEngine};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

fn usage() -> ! {
    eprintln!(
        "usage: agent-context-service --engine <dynamic|append|rolling>\n\
         \n\
         Speaks the context-contextcore wire protocol on stdin/stdout.\n"
    );
    std::process::exit(2);
}

fn build_engine(engine: &str) -> Arc<dyn ContextEngine> {
    match engine {
        "dynamic" => Arc::new(SimpleContextEngine::new(SimpleContextConfig::default())),
        "append" => Arc::new(AppendOnlyEngine::new()),
        "rolling" => Arc::new(RollingSummaryEngine::with_config(
            context_baselines::RollingConfig::default(),
        )),
        other => {
            eprintln!("unknown engine: {other}");
            usage();
        }
    }
}

async fn handle(op: ServiceOp, engine: &dyn ContextEngine) -> Result<Value, AgentError> {
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
        ServiceOp::Materialize { query } => {
            let materialized = engine.materialize(query).await?;
            serde_json::to_value(materialized).map_err(|e| AgentError::Context(e.to_string()))
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
        ServiceOp::Checkpoint => engine.checkpoint().await,
        ServiceOp::Restore { data } => {
            engine.restore(data).await?;
            Ok(Value::Null)
        }
        ServiceOp::Shutdown => Ok(Value::Null),
    }
}

#[tokio::main]
async fn main() {
    let mut engine = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--engine" => {
                let Some(value) = args.next() else {
                    usage();
                };
                engine = Some(build_engine(&value));
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    let engine = engine.expect("--engine is required");
    let engine: &dyn ContextEngine = engine.as_ref();

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let stdout = tokio::io::stdout();
    let mut writer = BufWriter::new(stdout);

    while let Ok(Some(line)) = lines.next_line().await {
        let request: ServiceRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = ServiceResponse::error(0, format!("bad request: {error}"));
                let _ = writer
                    .write_all(serde_json::to_string(&response).unwrap().as_bytes())
                    .await;
                let _ = writer.write_all(b"\n").await;
                let _ = writer.flush().await;
                continue;
            }
        };
        let id = request.id;
        if request.version != PROTOCOL_VERSION {
            let response = ServiceResponse::error(
                id,
                format!(
                    "protocol version mismatch: client {}, service {PROTOCOL_VERSION}",
                    request.version
                ),
            );
            let _ = writer
                .write_all(serde_json::to_string(&response).unwrap().as_bytes())
                .await;
            let _ = writer.write_all(b"\n").await;
            let _ = writer.flush().await;
            continue;
        }
        let shutdown = matches!(request.op, ServiceOp::Shutdown);
        let result = handle(request.op, engine).await;
        let response = match result {
            Ok(value) => ServiceResponse::ok(id, value),
            Err(error) => ServiceResponse::error(id, error.to_string()),
        };
        if writer
            .write_all(serde_json::to_string(&response).unwrap().as_bytes())
            .await
            .is_err()
        {
            break; // client is gone
        }
        if writer.write_all(b"\n").await.is_err() {
            break;
        }
        if writer.flush().await.is_err() {
            break;
        }
        if shutdown {
            break;
        }
    }
}
