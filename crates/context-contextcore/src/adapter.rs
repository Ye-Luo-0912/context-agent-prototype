//! `ContextEngine` implemented over a context-service process.
//!
//! This is the ContextCore integration shape: the kernel keeps talking to the
//! `ContextEngine` contract, and this adapter translates each call into a
//! JSON line on the service's stdin and awaits the response on its stdout.
//! The framed transport, deadlines and poisoned-connection policy live in
//! the shared `ProcessHost`; this module is only the protocol layer that
//! maps `ContextEngine` operations onto the service's wire operations.
//! Swapping the process behind the pipe (today: `agent-context-service`
//! running an in-process engine; tomorrow: a real ContextCore runtime) is a
//! composition-root detail — no kernel, tool, provider or UI code changes.

use std::sync::Arc;
use std::time::Duration;

use agent_contracts::{
    AgentError, AgentResult, ContextConsumptionAck, ContextDiagnostics, ContextEngine,
    ContextGcReport, ContextIngress, ContextItem, ContextItemId, ContextItemSummary,
    ContextMaintenanceReport, ContextMaintenanceTrigger, ContextQuery, ContextSearchCoverage,
    ContextSearchCoverageStop, ContextSearchObservation, ContextSearchQuery, ContextSearchResult,
    ContextStateTransition, ExternalizedContext, MaterializedContext, ScopeId, ScopeKind,
};
use async_trait::async_trait;
use serde_json::Value;

use crate::wire::{
    DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES, FEATURE_CONTEXT_SEARCH_REPORT,
    MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES, ServiceOp,
};
use agent_process::{ProcessHost, ProcessHostConfig, resolve_program};

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
    /// Where the service's context store lives. The composition root
    /// injects `workspace.state_dir()/context-store`; without it the
    /// service falls back to an OS temp dir — the store never lands in a
    /// CWD-relative path.
    pub store_dir: Option<std::path::PathBuf>,
    /// W2 (V3) parity affordance: pin the service engine's cold-paging
    /// knobs (restore card batch, hot-metadata entries, per-op hydrate
    /// items) below production defaults so boundary tests can reproduce a
    /// typed incomplete pass on a small fixture. `None` (production) keeps
    /// the service's own defaults.
    pub cold_paging: Option<(usize, usize, usize)>,
    pub startup_timeout: Duration,
    /// Deadline for every request after the handshake, so a wedged service
    /// cannot hang a turn.
    pub request_timeout: Duration,
    /// Hard cap on one request or response payload. The adapter passes the
    /// same limit to the child so neither side can grow memory without bound.
    pub max_frame_bytes: usize,
}

impl Default for ContextServiceConfig {
    fn default() -> Self {
        Self {
            program: None,
            engine: ServiceEngine::Dynamic,
            store_dir: None,
            cold_paging: None,
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            max_frame_bytes: DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES,
        }
    }
}

impl ContextServiceConfig {
    fn resolve_program(&self) -> String {
        if let Some(program) = &self.program {
            return program.clone();
        }
        let name = if cfg!(windows) {
            "agent-context-service.exe"
        } else {
            "agent-context-service"
        };
        resolve_program(Some("CARGO_BIN_EXE_agent-context-service"), name)
    }
}

/// A `ContextEngine` whose state lives in a separate process.
pub struct ContextServiceAdapter {
    host: ProcessHost,
    /// W2 (V3): compatibility mirror of the most recent atomic search
    /// report, kept ONLY so the legacy `last_search_coverage` /
    /// `last_search_observation` side-channel methods stop answering with
    /// the trait's `complete` default (a fabricated fact). The production
    /// path is `search_external_report`, which returns the pass's facts
    /// atomically and never reads this back.
    last_report: std::sync::Mutex<Option<ContextSearchResult>>,
}

impl ContextServiceAdapter {
    /// Spawn the service, handshake, and return a ready adapter.
    pub async fn connect(config: &ContextServiceConfig) -> AgentResult<Self> {
        if !(MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES..=DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES)
            .contains(&config.max_frame_bytes)
        {
            return Err(AgentError::InvalidRequest(format!(
                "context service frame bound must be in {MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES}..={DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES} bytes"
            )));
        }
        let program = config.resolve_program();
        let mut args = vec!["--engine".into(), config.engine.as_arg().into()];
        // The store must live under the workspace state dir, never a
        // CWD-relative path the child happens to run in.
        if let Some(dir) = &config.store_dir {
            args.push("--store-dir".into());
            args.push(dir.to_string_lossy().into_owned());
        }
        if let Some((restore, hot, items)) = config.cold_paging {
            args.push("--cold-paging".into());
            args.push(format!("{restore},{hot},{items}"));
        }
        args.push("--max-frame-bytes".into());
        args.push(config.max_frame_bytes.to_string());
        let host = ProcessHost::connect(ProcessHostConfig {
            program: program.clone(),
            args,
            env: Vec::new(),
            startup_timeout: config.startup_timeout,
            request_timeout: config.request_timeout,
            max_frame_bytes: config.max_frame_bytes,
            // Strict ping-pong with no system frames: one call moves at most
            // a request frame plus the response frame.
            max_call_bytes: config.max_frame_bytes.saturating_mul(2).saturating_add(2),
            // No broker is installed for this boundary; the bound is a
            // placeholder for the shared host's control-plane cap.
            max_system_answer_bytes: 512 * 1024,
            // W2 (V3): offer the atomic-search capability. The service's
            // pong advertises what IT supports and the host intersects;
            // a service that did not advertise it leaves
            // `search_external_report` explicitly Unsupported.
            offered_features: agent_platform_protocol::ActiveFeatures::new(vec![
                FEATURE_CONTEXT_SEARCH_REPORT.to_string(),
            ])
            .unwrap_or_default(),
            // The context service is the runtime's own trusted sidecar; it
            // keeps the historical inherit-all behavior. The strict sandbox
            // is applied to *capabilities* (see agent-capability-process).
            sandbox: agent_process::ProcessSandbox::default(),
        })
        .await
        .map_err(|e| {
            AgentError::Context(format!(
                "spawn context service '{program}': {e} (build it with `cargo build -p agent-context-service`)"
            ))
        })?;
        Ok(Self {
            host,
            last_report: std::sync::Mutex::new(None),
        })
    }

    async fn call(&self, op: ServiceOp) -> AgentResult<Value> {
        let op_value = serde_json::to_value(op)
            .map_err(|e| AgentError::Context(format!("serialize request: {e}")))?;
        self.host.call(op_value).await
    }

    /// W2 (V3): whether the handshake intersected the atomic-search
    /// capability on this connection.
    fn search_report_negotiated(&self) -> bool {
        self.host.allows_feature(FEATURE_CONTEXT_SEARCH_REPORT)
    }

    /// Graceful stop: ask the service to exit, then reap it.
    pub async fn shutdown(self) {
        self.host.shutdown().await;
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

    async fn gc(&self) -> AgentResult<ContextGcReport> {
        let value = self.call(ServiceOp::Gc).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode gc report: {e}")))
    }

    async fn storage_gc(&self) -> AgentResult<agent_contracts::StorageGcReport> {
        let value = self.call(ServiceOp::StorageGc).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode storage gc report: {e}")))
    }

    async fn reconcile_store(&self) -> AgentResult<agent_contracts::StoreReconcileReport> {
        let value = self.call(ServiceOp::ReconcileStore).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode store reconcile report: {e}")))
    }

    async fn materialize(&self, query: ContextQuery) -> AgentResult<MaterializedContext> {
        let value = self.call(ServiceOp::Materialize { query }).await?;
        let materialized: MaterializedContext = serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode materialized context: {e}")))?;
        // Cross-process adapters bypass the in-process engine boundary, so
        // the service response is re-validated here before it can reach the
        // durable event stream or the provider.
        materialized
            .validate_materialization()
            .map_err(|e| AgentError::Context(format!("materialized context failed: {e}")))?;
        Ok(materialized)
    }

    async fn acknowledge_consumption(&self, ack: ContextConsumptionAck) -> AgentResult<()> {
        self.call(ServiceOp::AcknowledgeConsumption { ack }).await?;
        Ok(())
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

    async fn search_external(
        &self,
        query: ContextSearchQuery,
    ) -> AgentResult<Vec<ExternalizedContext>> {
        // Keep direct process-adapter callers on the same semantic bounds as
        // Core and the in-process engine. The service validates again at its
        // own engine boundary; this also bounds the outgoing wire request.
        //
        // W2 (V3): when the service negotiated the atomic capability, the
        // legacy methods route through it too, so every search on this
        // boundary updates the compatibility mirror coherently. The legacy
        // op remains for services without the capability — with the honest
        // consequence that no coverage facts are available (see
        // `last_search_coverage`).
        if self.search_report_negotiated() {
            return Ok(self.search_external_report(query, None).await?.hits);
        }
        let value = self
            .call(ServiceOp::SearchExternal {
                query: query.normalized(),
            })
            .await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode external context search: {e}")))
    }

    async fn search_external_continuation(
        &self,
        query: ContextSearchQuery,
        continuation: &str,
    ) -> AgentResult<Vec<ExternalizedContext>> {
        // W2 (V3): a negotiated service forwards the token to its real
        // continuation implementation. Without the capability the token
        // cannot cross this boundary honestly, so this is an explicit
        // Unsupported error — silently ignoring it would degrade a resumed
        // search into a fresh one and lose the walk semantics.
        if self.search_report_negotiated() {
            return Ok(self
                .search_external_report(query, Some(continuation))
                .await?
                .hits);
        }
        Err(AgentError::Context(format!(
            "context service did not negotiate '{FEATURE_CONTEXT_SEARCH_REPORT}': search \
             continuation tokens cannot be forwarded across this boundary"
        )))
    }

    /// W2 (V3): the atomic search — hits, coverage and observation of
    /// exactly one pass, forwarded from the serving engine. This is the
    /// channel Core uses; no read-after-call side channel is involved.
    async fn search_external_report(
        &self,
        query: ContextSearchQuery,
        continuation: Option<&str>,
    ) -> AgentResult<ContextSearchResult> {
        if !self.search_report_negotiated() {
            // An old service cannot prove coverage; the honest answer is an
            // explicit Unsupported, never a defaulted `complete`.
            return Err(AgentError::Context(format!(
                "context service did not negotiate '{FEATURE_CONTEXT_SEARCH_REPORT}': atomic \
                 search facts (coverage/continuation) are unsupported on this boundary"
            )));
        }
        let value = self
            .call(ServiceOp::SearchExternalReport {
                query: query.normalized(),
                continuation: continuation.map(str::to_string),
            })
            .await?;
        let report: ContextSearchResult = serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode external search report: {e}")))?;
        // Compatibility mirror for the legacy side-channel methods below.
        // Bounded (one report) and write-only here: the production path
        // returns the value directly.
        match self.last_report.lock() {
            Ok(mut slot) => *slot = Some(report.clone()),
            Err(poisoned) => *poisoned.into_inner() = Some(report.clone()),
        }
        Ok(report)
    }

    /// S3 + W2 (V3): the coverage facts of the most recent search this
    /// adapter actually observed. Before any search (or against a service
    /// without the capability) the honest answer is `Unknown` — this
    /// boundary cannot self-certify `Complete` the way a fully in-memory
    /// baseline can.
    fn last_search_coverage(&self) -> ContextSearchCoverage {
        let guard = match self.last_report.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        match guard.as_ref() {
            Some(report) => report.coverage.clone(),
            None => ContextSearchCoverage {
                complete: false,
                unread_pages: 0,
                stop: ContextSearchCoverageStop::Unknown,
                continuation: None,
            },
        }
    }

    fn last_search_observation(&self) -> ContextSearchObservation {
        let guard = match self.last_report.lock() {
            Ok(slot) => slot,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .as_ref()
            .map_or_else(Default::default, |r| r.observation)
    }

    async fn inspect_external(
        &self,
        item_id: ContextItemId,
    ) -> AgentResult<Option<ExternalizedContext>> {
        let value = self.call(ServiceOp::InspectExternal { item_id }).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode external context metadata: {e}")))
    }

    async fn fetch_external(&self, item_id: ContextItemId) -> AgentResult<Option<ContextItem>> {
        let value = self.call(ServiceOp::FetchExternal { item_id }).await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode external context item: {e}")))
    }

    /// EXEC-9 (R3-01): root parsing runs in the service process with the
    /// engine's real, versioned implementation — never an empty-set guess.
    /// A wire failure is an `Err`, which the runtime reports as an
    /// INCOMPLETE root set (deletion defers) instead of "nothing retained".
    async fn checkpoint_recovery_item_ids(
        &self,
        checkpoint: &serde_json::Value,
    ) -> AgentResult<Vec<ContextItemId>> {
        let value = self
            .call(ServiceOp::CheckpointRecoveryItemIds {
                checkpoint: checkpoint.clone(),
            })
            .await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode checkpoint recovery roots: {e}")))
    }

    /// EXEC-9 (R3-01): the protecting Storage GC crosses the wire with its
    /// roots and completeness flag; the service-side engine owns the real
    /// deletion decision under the shared retained-root invariant.
    async fn storage_gc_protecting(
        &self,
        protected_recovery_roots: &[ContextItemId],
        roots_complete: bool,
    ) -> AgentResult<agent_contracts::StorageGcReport> {
        let value = self
            .call(ServiceOp::StorageGcProtecting {
                roots: protected_recovery_roots.to_vec(),
                roots_complete,
            })
            .await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode protected storage gc report: {e}")))
    }

    /// EXEC-9 (R3-01): the protecting reconcile crosses the wire with its
    /// roots and completeness flag; the service-side engine owns the real
    /// stale-duplicate decision under the shared retained-root invariant.
    async fn reconcile_store_protecting(
        &self,
        protected: &[ContextItemId],
        roots_complete: bool,
    ) -> AgentResult<agent_contracts::StoreReconcileReport> {
        let value = self
            .call(ServiceOp::ReconcileStoreProtecting {
                roots: protected.to_vec(),
                roots_complete,
            })
            .await?;
        serde_json::from_value(value)
            .map_err(|e| AgentError::Context(format!("decode protected reconcile report: {e}")))
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
