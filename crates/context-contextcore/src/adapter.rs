//! `ContextEngine` implemented over a context-service process.
//!
//! This is the ContextCore integration shape: the kernel keeps talking to the
//! `ContextEngine` contract, and this adapter translates each call into a
//! JSON line on the service's stdin and awaits the response on its stdout.
//! Swapping the process behind the pipe (today: `agent-context-service`
//! running an in-process engine; tomorrow: a real ContextCore runtime) is a
//! composition-root detail — no kernel, tool, provider or UI code changes.

use std::{
    io::ErrorKind,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use agent_contracts::{
    AgentError, AgentResult, ContextDiagnostics, ContextEngine, ContextIngress, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextStateTransition,
    MaterializedContext, ScopeId, ScopeKind,
};
use async_trait::async_trait;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};

use crate::wire::{ServiceOp, ServiceRequest, ServiceResponse};

/// Which engine the spawned service should run. The adapter is agnostic; the
/// choice belongs to the composition root (same as `--context=` in the TUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceEngine {
    Dynamic,
    Append,
    Rolling,
}

impl ServiceEngine {
    pub fn as_arg(self) -> &'static str {
        match self {
            ServiceEngine::Dynamic => "dynamic",
            ServiceEngine::Append => "append",
            ServiceEngine::Rolling => "rolling",
        }
    }
}

/// How to reach the context-service executable.
#[derive(Debug, Clone)]
pub struct ContextServiceConfig {
    /// Binary name or path. Resolved in order: `ContextServiceConfig::program`
    /// if set, else `CARGO_BIN_EXE_agent-context-service` (integration tests),
    /// else a sibling of the current executable, else PATH lookup.
    pub program: Option<String>,
    pub engine: ServiceEngine,
    pub startup_timeout: Duration,
}

impl Default for ContextServiceConfig {
    fn default() -> Self {
        Self {
            program: None,
            engine: ServiceEngine::Dynamic,
            startup_timeout: Duration::from_secs(10),
        }
    }
}

impl ContextServiceConfig {
    fn resolve_program(&self) -> String {
        if let Some(program) = &self.program {
            return program.clone();
        }
        if let Ok(program) = std::env::var("CARGO_BIN_EXE_agent-context-service") {
            return program;
        }
        if let Ok(current) = std::env::current_exe() {
            let exe = current.file_name().unwrap_or_default().to_string_lossy();
            let sibling_name = if exe.ends_with(".exe") {
                "agent-context-service.exe".to_string()
            } else {
                "agent-context-service".to_string()
            };
            if let Some(parent) = current.parent() {
                let sibling = parent.join(&sibling_name);
                if sibling.exists() {
                    return sibling.to_string_lossy().into_owned();
                }
            }
        }
        "agent-context-service".to_string()
    }
}

/// A `ContextEngine` whose state lives in a separate process.
pub struct ContextServiceAdapter {
    child: Child,
    /// Strict ping-pong: one request in flight at a time. Held in a `Mutex`
    /// because the `ContextEngine` trait only gives `&self`.
    io: Mutex<AdapterIo>,
    next_id: AtomicU64,
}

struct AdapterIo {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl ContextServiceAdapter {
    /// Spawn the service, handshake, and return a ready adapter.
    pub async fn connect(config: &ContextServiceConfig) -> AgentResult<Self> {
        let program = config.resolve_program();
        let mut child = Command::new(&program)
            .arg("--engine")
            .arg(config.engine.as_arg())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                AgentError::Context(format!(
                    "spawn context service '{program}': {e} (build it with `cargo build -p agent-context-service`)"
                ))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Context("context service stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Context("context service stdout unavailable".into()))?;

        let adapter = Self {
            child,
            io: Mutex::new(AdapterIo {
                stdin,
                stdout: BufReader::new(stdout),
            }),
            next_id: AtomicU64::new(1),
        };

        // Fail fast on a missing/broken service instead of on the first turn.
        timeout(config.startup_timeout, adapter.call(ServiceOp::Ping))
            .await
            .map_err(|_| AgentError::Context("context service did not respond to ping".into()))?
            .map_err(|e| AgentError::Context(format!("context service handshake: {e}")))?;

        Ok(adapter)
    }

    async fn call(&self, op: ServiceOp) -> AgentResult<Value> {
        let mut io = self.io.lock().await;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = ServiceRequest { id, op };
        let line = serde_json::to_string(&request)
            .map_err(|e| AgentError::Context(format!("serialize request: {e}")))?;

        io.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(service_io_error)?;
        io.stdin.write_all(b"\n").await.map_err(service_io_error)?;
        io.stdin.flush().await.map_err(service_io_error)?;

        let mut line = String::new();
        let read = io
            .stdout
            .read_line(&mut line)
            .await
            .map_err(service_io_error)?;
        if read == 0 {
            return Err(AgentError::Context(
                "context service closed its stdout (did it crash?)".into(),
            ));
        }
        let response: ServiceResponse = serde_json::from_str(&line).map_err(|e| {
            AgentError::Context(format!("parse service response: {e} (line: {line})"))
        })?;
        if response.id != id {
            return Err(AgentError::Context(format!(
                "service response id mismatch: got {}, expected {id}",
                response.id
            )));
        }
        if !response.ok {
            return Err(AgentError::Context(
                response
                    .error
                    .unwrap_or_else(|| "unknown service error".into()),
            ));
        }
        Ok(response.value)
    }

    /// Graceful stop: ask the service to exit, then reap it.
    pub async fn shutdown(mut self) {
        let _ = self.call(ServiceOp::Shutdown).await;
        let _ = self.child.wait().await;
    }
}

fn service_io_error(e: std::io::Error) -> AgentError {
    if e.kind() == ErrorKind::BrokenPipe || e.kind() == ErrorKind::UnexpectedEof {
        AgentError::Context(format!("context service connection closed: {e}"))
    } else {
        AgentError::Io(format!("context service io: {e}"))
    }
}

#[async_trait]
impl ContextEngine for ContextServiceAdapter {
    async fn ingest(&self, ingress: ContextIngress) -> AgentResult<()> {
        self.call(ServiceOp::Ingest { ingress }).await?;
        Ok(())
    }

    async fn maintain(
        &self,
        trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        let value = self.call(ServiceOp::Maintain { trigger }).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode maintain report: {e}")))
    }

    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        let value = self.call(ServiceOp::Materialize { query }).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode materialized context: {e}")))
    }

    async fn open_scope(&self, kind: ScopeKind, parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        let value = self.call(ServiceOp::OpenScope { kind, parent }).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode scope id: {e}")))
    }

    async fn close_scope(&self, scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        let value = self.call(ServiceOp::CloseScope { scope_id }).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode scope close: {e}")))
    }

    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        let value = self.call(ServiceOp::Diagnostics).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode diagnostics: {e}")))
    }

    async fn inspect(&self, limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        let value = self.call(ServiceOp::Inspect { limit }).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode inspect: {e}")))
    }

    async fn checkpoint(&self) -> AgentResult<Value> {
        self.call(ServiceOp::Checkpoint).await
    }

    async fn restore(&self, data: Value) -> AgentResult<()> {
        self.call(ServiceOp::Restore { data }).await?;
        Ok(())
    }
}

/// Convenience: `Arc<dyn ContextEngine>` for a spawned service.
pub async fn connect_engine(config: &ContextServiceConfig) -> AgentResult<Arc<dyn ContextEngine>> {
    Ok(Arc::new(ContextServiceAdapter::connect(config).await?))
}
