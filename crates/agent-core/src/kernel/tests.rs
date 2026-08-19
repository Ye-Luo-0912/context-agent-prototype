use super::*;
use agent_contracts::{
    AccessSignal, ApprovalDecision, ApprovalGate, AttentionState, ContextDiagnostics,
    ContextIngress, ContextItem, ContextItemId, ContextItemSummary, ContextKind,
    ContextMaintenanceReport, ContextQuery, ContextRef, ContextResidency, ContextRetention,
    ContextScope, ContextStateTransition, ExternalizedContext, MaterializedContext, ScopeId,
    ScopeKind, SemanticState, ToolRisk, ToolSemanticRole, ToolSpec, ToolSurfaceDemand,
    ToolSurfaceOmission, ToolSurfaceOmissionReason, TurnId,
};

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: name.into(),
        arguments: serde_json::json!({}),
    }
}

fn operation_identity(
    kernel: &CoreAuthority,
    call: &ToolCall,
    generation: u64,
) -> ToolOperationIdentity {
    ToolOperationIdentity {
        run_id: kernel.run_id(),
        task_id: None,
        turn_id: TurnId::new(),
        scope_id: None,
        operation_id: OperationId::new(),
        generation,
        call_id: call.id.clone(),
        tool_name: call.name.clone(),
        argument_digest: ArgumentDigest::from_json(&call.arguments),
    }
}

#[test]
fn off_surface_rejection_uses_only_the_captured_omission_reason() {
    let surface = ToolSurfaceSnapshot {
        surface_revision: 9,
        omissions: vec![ToolSurfaceOmission {
            tool_name: "optional.large".into(),
            demand: ToolSurfaceDemand::PreferSurface,
            origin: agent_contracts::ToolSurfaceOrigin::CatalogLoadedOptional,
            reason: ToolSurfaceOmissionReason::ProviderInputBudget,
            approx_tokens: 2_500,
        }],
        omitted_total: 1,
        ..ToolSurfaceSnapshot::default()
    };

    let message = tool_not_on_surface_message(&call("optional.large"), &surface);

    assert!(message.contains("model surface revision 9"));
    assert!(message.contains("provider input budget"));
    assert!(!message.contains("capability.manage"));
    assert!(!message.contains("load"));
}

#[test]
fn unrecorded_off_surface_call_is_rejected_without_live_catalog_claims() {
    let surface = ToolSurfaceSnapshot {
        surface_revision: 12,
        omitted_total: 7,
        ..ToolSurfaceSnapshot::default()
    };

    let message = tool_not_on_surface_message(&call("unlisted.tool"), &surface);

    assert!(message.contains("model surface revision 12"));
    assert!(message.contains("only schemas in that captured surface may be called"));
    assert!(!message.contains("unknown tool"));
    assert!(!message.contains("loaded"));
}

// --- CORE-04: trusted output broker + execution-enforced query limits ---

#[derive(Default)]
struct RecordingBroker {
    calls: std::sync::Mutex<usize>,
    last_output: std::sync::Mutex<Option<ToolOutput>>,
}

#[async_trait::async_trait]
impl OutputBroker for RecordingBroker {
    async fn bound(
        &self,
        _run_id: RunId,
        _budget: Option<usize>,
        output: ToolOutput,
    ) -> ToolOutput {
        *self.calls.lock().unwrap() += 1;
        *self.last_output.lock().unwrap() = Some(output.clone());
        output
    }
}

struct BigOutputDispatcher {
    output: ToolOutput,
}

#[async_trait::async_trait]
impl ToolDispatcher for BigOutputDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "big.tool".into(),
            description: "returns oversized output".into(),
            input_schema: serde_json::json!({}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }]
    }
    async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        assert_eq!(request.call.name, "big.tool");
        Ok(ToolOutcome::Value(self.output.clone()))
    }
}

/// Returns a fixed output for any call — for tests that exercise the
/// approval/lease path without caring about the dispatched value.
struct EchoDispatcher {
    output: ToolOutput,
}

#[async_trait::async_trait]
impl ToolDispatcher for EchoDispatcher {
    fn specs(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    async fn execute(&self, _request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
        Ok(ToolOutcome::Value(self.output.clone()))
    }
}

struct AllowAllApproval;

#[async_trait::async_trait]
impl ApprovalGate for AllowAllApproval {
    async fn authorize(
        &self,
        _call: &ToolCall,
        _spec: &ToolSpec,
        _cancel: &CancellationToken,
    ) -> AgentResult<ApprovalDecision> {
        Ok(ApprovalDecision::Allow)
    }
}

#[derive(Default)]
struct RecordingEngine {
    searched_limits: std::sync::Mutex<Vec<usize>>,
    searched_queries: std::sync::Mutex<Vec<String>>,
    search_hits: std::sync::Mutex<Vec<ExternalizedContext>>,
    inspect_external_entry: std::sync::Mutex<Option<ExternalizedContext>>,
    inspect_summaries: std::sync::Mutex<Vec<ContextItemSummary>>,
    fetched: std::sync::Mutex<Option<ContextItem>>,
    search_error: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl ContextEngine for RecordingEngine {
    async fn ingest(&self, _ingress: ContextIngress) -> AgentResult<()> {
        unimplemented!()
    }
    async fn maintain(
        &self,
        _trigger: ContextMaintenanceTrigger,
    ) -> AgentResult<ContextMaintenanceReport> {
        unimplemented!()
    }
    async fn materialize(&self, _query: ContextQuery) -> AgentResult<MaterializedContext> {
        unimplemented!()
    }
    async fn open_scope(&self, _kind: ScopeKind, _parent: Option<ScopeId>) -> AgentResult<ScopeId> {
        unimplemented!()
    }
    async fn close_scope(&self, _scope_id: ScopeId) -> AgentResult<Vec<ContextStateTransition>> {
        unimplemented!()
    }
    async fn diagnostics(&self) -> AgentResult<ContextDiagnostics> {
        unimplemented!()
    }
    async fn inspect(&self, _limit: usize) -> AgentResult<Vec<ContextItemSummary>> {
        Ok(self.inspect_summaries.lock().unwrap().clone())
    }
    async fn search_external(
        &self,
        query: ContextSearchQuery,
    ) -> AgentResult<Vec<ExternalizedContext>> {
        if let Some(reason) = self.search_error.lock().unwrap().clone() {
            return Err(AgentError::Context(reason));
        }
        self.searched_limits.lock().unwrap().push(query.limit);
        self.searched_queries
            .lock()
            .unwrap()
            .push(query.query.clone());
        Ok(self.search_hits.lock().unwrap().clone())
    }
    async fn inspect_external(
        &self,
        _item_id: ContextItemId,
    ) -> AgentResult<Option<ExternalizedContext>> {
        Ok(self.inspect_external_entry.lock().unwrap().clone())
    }
    async fn fetch_external(&self, _item_id: ContextItemId) -> AgentResult<Option<ContextItem>> {
        Ok(self.fetched.lock().unwrap().clone())
    }
    async fn checkpoint(&self) -> AgentResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
    async fn restore(&self, _data: serde_json::Value) -> AgentResult<()> {
        Ok(())
    }
}

fn test_item(content: String) -> ContextItem {
    ContextItem {
        id: ContextItemId::new(),
        task_id: None,
        scope_id: None,
        content,
        kind: ContextKind::Note,
        scope: ContextScope::Task,
        retention: ContextRetention::Working,
        attention: AttentionState::Active,
        semantic: SemanticState::Live,
        importance: 0.0,
        relevance: 0.0,
        created_tick: 0,
        last_access_tick: 0,
        access_count: 0,
        created_turn: 0,
        last_access_turn: 0,
        last_selected_turn: 0,
        dependencies: Vec::new(),
        tags: Vec::new(),
        keep_alive: false,
        lease_until_turn: None,
        source: None,
        residency: ContextResidency::Resident,
        gc_generation: 0,
        evicted_at_tick: None,
        entities: Vec::new(),
        file_path: None,
        file_revision: None,
    }
}

fn test_kernel(
    engine: Arc<dyn ContextEngine>,
    dispatcher: Arc<dyn ToolDispatcher>,
    broker: Option<Arc<dyn OutputBroker>>,
) -> Arc<CoreAuthority> {
    Arc::new(CoreAuthority::new(
        CoreAuthorityConfig {
            output_broker: broker,
            ..CoreAuthorityConfig::default()
        },
        engine,
        dispatcher,
        Arc::new(AllowAllApproval),
        None,
        None,
    ))
}

/// 构造一个带指定来源权威（source）的外部化条目，用于检索输出渲染测试。
fn external_entry(source: Option<&str>) -> ExternalizedContext {
    let item_id = ContextItemId::new();
    ExternalizedContext {
        item_id,
        task_id: None,
        scope_id: None,
        kind: ContextKind::Note,
        scope: ContextScope::Task,
        retention: ContextRetention::Working,
        attention: AttentionState::Archived,
        semantic: SemanticState::Live,
        context_ref: ContextRef {
            uri: format!("context://run/{item_id}"),
            item_id,
            kind: ContextKind::Note,
            scope: ContextScope::Task,
            summary: "a past tool capture".into(),
            created_tick: 0,
        },
        externalized_at_tick: 0,
        last_access_tick: 0,
        residency: ContextResidency::Cold,
        entities: Vec::new(),
        tags: Vec::new(),
        dependencies: Vec::new(),
        keep_alive: false,
        lease_until_turn: None,
        last_access_gc_epoch: Some(0),
        blob_checksum: None,
        source: source.map(|s| s.to_string()),
        importance: 0.0,
        relevance: 0.0,
        created_tick: 0,
        created_turn: 0,
        last_access_turn: 0,
        last_selected_turn: 0,
        access_count: 0,
        last_access_signal: AccessSignal::None,
        search_reinforce_count: 0,
        gc_generation: 0,
        evicted_at_tick: None,
        file_path: None,
        file_revision: None,
    }
}

fn surface_with(name: &str) -> ToolSurfaceSnapshot {
    ToolSurfaceSnapshot {
        specs: vec![ToolSpec {
            name: name.into(),
            description: "x".into(),
            input_schema: serde_json::json!({}),
            risk: ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }],
        ..ToolSurfaceSnapshot::default()
    }
}

#[tokio::test]
async fn output_broker_bounds_tool_results_before_the_actor() {
    let broker = Arc::new(RecordingBroker::default());
    let dispatcher = Arc::new(BigOutputDispatcher {
        output: ToolOutput {
            call_id: "c1".into(),
            tool_name: "big.tool".into(),
            ok: true,
            summary: "done".into(),
            model_content: "x".repeat(100_000),
            artifact_ref: None,
            metadata: serde_json::Value::Null,
        },
    });
    let kernel = test_kernel(
        Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
            inspect_summaries: Default::default(),
            search_error: Default::default(),
            fetched: Default::default(),
        }),
        dispatcher,
        Some(broker.clone()),
    );
    let tool_call = call("big.tool");
    let generation = kernel.current_authority_epoch();
    let execution = kernel
        .execute_tool(
            operation_identity(&kernel, &tool_call, generation),
            tool_call,
            CancellationToken::new(),
            &surface_with("big.tool"),
            generation,
        )
        .await;
    let crate::port::CoreToolExecution { outcome, lease, .. } = execution;
    assert!(
        lease.is_none(),
        "a read-only call carries no commit-time lease"
    );
    assert_eq!(*broker.calls.lock().unwrap(), 1, "broker must run once");
    let ToolOutcome::Value(output) = outcome else {
        panic!("expected a plain value");
    };
    assert_eq!(output.model_content.len(), 100_000);
    let seen = broker
        .last_output
        .lock()
        .unwrap()
        .clone()
        .expect("broker saw the output");
    assert_eq!(seen.model_content, "x".repeat(100_000));
}

#[tokio::test]
async fn no_broker_keeps_the_outcome_untouched() {
    let dispatcher = Arc::new(BigOutputDispatcher {
        output: ToolOutput {
            call_id: "c1".into(),
            tool_name: "big.tool".into(),
            ok: true,
            summary: "done".into(),
            model_content: "x".repeat(100_000),
            artifact_ref: None,
            metadata: serde_json::Value::Null,
        },
    });
    let kernel = test_kernel(
        Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
            inspect_summaries: Default::default(),
            search_error: Default::default(),
            fetched: Default::default(),
        }),
        dispatcher,
        None,
    );
    let tool_call = call("big.tool");
    let generation = kernel.current_authority_epoch();
    let execution = kernel
        .execute_tool(
            operation_identity(&kernel, &tool_call, generation),
            tool_call,
            CancellationToken::new(),
            &surface_with("big.tool"),
            generation,
        )
        .await;
    let crate::port::CoreToolExecution { outcome, lease, .. } = execution;
    assert!(
        lease.is_none(),
        "a read-only call carries no commit-time lease"
    );
    let ToolOutcome::Value(output) = outcome else {
        panic!("expected a plain value");
    };
    assert_eq!(output.model_content.len(), 100_000);
}

#[tokio::test]
async fn context_fetch_results_are_bounded_after_resolve() {
    let broker = Arc::new(RecordingBroker::default());
    let engine = Arc::new(RecordingEngine {
        searched_limits: Default::default(),
        searched_queries: Default::default(),
        search_hits: Default::default(),
        inspect_external_entry: Default::default(),
        inspect_summaries: Default::default(),
        search_error: Default::default(),
        fetched: std::sync::Mutex::new(Some(test_item("big".repeat(200_000)))),
    });
    let kernel = test_kernel(
        engine,
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        Some(broker.clone()),
    );
    let placeholder = ToolOutput {
        call_id: "c1".into(),
        tool_name: "context.manage".into(),
        ok: true,
        summary: "placeholder".into(),
        model_content: "placeholder".into(),
        artifact_ref: None,
        metadata: serde_json::Value::Null,
    };
    let output = kernel
        .resolve_engine_query(
            placeholder,
            EngineQuery::FetchExternal {
                item_id: ContextItemId::new(),
            },
        )
        .await;
    assert_eq!(
        *broker.calls.lock().unwrap(),
        1,
        "broker must bound the fetch result"
    );
    assert!(output.model_content.contains("big"));
    let seen = broker
        .last_output
        .lock()
        .unwrap()
        .clone()
        .expect("broker saw the output");
    assert!(
        seen.model_content.contains("big"),
        "the full fetched content reaches the broker"
    );
}

#[tokio::test]
async fn search_limit_is_clamped_in_execution() {
    let engine = Arc::new(RecordingEngine {
        searched_limits: Default::default(),
        searched_queries: Default::default(),
        search_hits: Default::default(),
        inspect_external_entry: Default::default(),
        inspect_summaries: Default::default(),
        search_error: Default::default(),
        fetched: Default::default(),
    });
    let kernel = test_kernel(
        engine.clone(),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        None,
    );
    let placeholder = ToolOutput {
        call_id: "c1".into(),
        tool_name: "context.manage".into(),
        ok: true,
        summary: "placeholder".into(),
        model_content: "placeholder".into(),
        artifact_ref: None,
        metadata: serde_json::Value::Null,
    };
    let _ = kernel
        .resolve_engine_query(
            placeholder.clone(),
            EngineQuery::SearchExternal {
                query: "x".into(),
                kind: None,
                scope: None,
                task_id: None,
                label: None,
                limit: 1_000_000,
            },
        )
        .await;
    let limits = engine.searched_limits.lock().unwrap();
    assert_eq!(limits.as_slice(), &[CONTEXT_SEARCH_MAX_LIMIT]);
}

#[tokio::test]
async fn search_limit_zero_keeps_the_engine_default() {
    let engine = Arc::new(RecordingEngine {
        searched_limits: Default::default(),
        searched_queries: Default::default(),
        search_hits: Default::default(),
        inspect_external_entry: Default::default(),
        inspect_summaries: Default::default(),
        search_error: Default::default(),
        fetched: Default::default(),
    });
    let kernel = test_kernel(
        engine.clone(),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        None,
    );
    let placeholder = ToolOutput {
        call_id: "c1".into(),
        tool_name: "context.manage".into(),
        ok: true,
        summary: "placeholder".into(),
        model_content: "placeholder".into(),
        artifact_ref: None,
        metadata: serde_json::Value::Null,
    };
    let _ = kernel
        .resolve_engine_query(
            placeholder,
            EngineQuery::SearchExternal {
                query: "x".into(),
                kind: None,
                scope: None,
                task_id: None,
                label: None,
                limit: 0,
            },
        )
        .await;
    let limits = engine.searched_limits.lock().unwrap();
    assert_eq!(
        limits.as_slice(),
        &[0],
        "0 must stay 0 so the engine default applies"
    );
}

#[tokio::test]
async fn search_query_length_is_bounded_in_execution() {
    // 超长查询在执行期被截断到 CONTEXT_SEARCH_MAX_QUERY_CHARS：
    // 引擎只收到有界长度的查询字符串，模型无法用巨型查询冲刷检索路径。
    let engine = Arc::new(RecordingEngine {
        searched_limits: Default::default(),
        searched_queries: Default::default(),
        search_hits: Default::default(),
        inspect_external_entry: Default::default(),
        inspect_summaries: Default::default(),
        search_error: Default::default(),
        fetched: Default::default(),
    });
    let kernel = test_kernel(
        engine.clone(),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        None,
    );
    let placeholder = ToolOutput {
        call_id: "c1".into(),
        tool_name: "context.manage".into(),
        ok: true,
        summary: "placeholder".into(),
        model_content: "placeholder".into(),
        artifact_ref: None,
        metadata: serde_json::Value::Null,
    };
    let _ = kernel
        .resolve_engine_query(
            placeholder,
            EngineQuery::SearchExternal {
                query: "x".repeat(CONTEXT_SEARCH_MAX_QUERY_CHARS * 4),
                kind: None,
                scope: None,
                task_id: None,
                label: None,
                limit: 10,
            },
        )
        .await;
    let queries = engine.searched_queries.lock().unwrap();
    assert_eq!(queries.len(), 1);
    assert_eq!(
        queries[0].chars().count(),
        CONTEXT_SEARCH_MAX_QUERY_CHARS,
        "the engine must receive a query truncated to the execution cap"
    );
}

#[tokio::test]
async fn empty_search_distinguishes_no_evidence_from_filter_miss() {
    // 无过滤与带过滤的空结果文案不同，方便换条件，不写工作集说明书。
    let engine = Arc::new(RecordingEngine {
        searched_limits: Default::default(),
        searched_queries: Default::default(),
        search_hits: Default::default(),
        inspect_external_entry: Default::default(),
        inspect_summaries: Default::default(),
        search_error: Default::default(),
        fetched: Default::default(),
    });
    let kernel = test_kernel(
        engine.clone(),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        None,
    );
    let placeholder = ToolOutput {
        call_id: "c1".into(),
        tool_name: "context.manage".into(),
        ok: true,
        summary: "placeholder".into(),
        model_content: "placeholder".into(),
        artifact_ref: None,
        metadata: serde_json::Value::Null,
    };
    let no_filter = kernel
        .resolve_engine_query(
            placeholder.clone(),
            EngineQuery::SearchExternal {
                query: "x".into(),
                kind: None,
                scope: None,
                task_id: None,
                label: None,
                limit: 10,
            },
        )
        .await;
    assert!(
        no_filter.model_content.contains("no catalog items match"),
        "no filter must report that there is genuinely no catalog evidence"
    );
    let filtered = kernel
        .resolve_engine_query(
            placeholder,
            EngineQuery::SearchExternal {
                query: "x".into(),
                kind: Some(ContextKind::Note),
                scope: None,
                task_id: None,
                label: None,
                limit: 10,
            },
        )
        .await;
    assert!(
        filtered.model_content.contains("requested filter"),
        "a filter miss must name the filter, not absent evidence: {}",
        filtered.model_content
    );
}

#[tokio::test]
async fn search_hits_render_the_source_authority() {
    // 检索命中行携带来源权威：带 source 的条目显示真实来源，
    // 无来源的条目显示 "-"，与 task 占位风格一致。
    let engine = Arc::new(RecordingEngine {
        searched_limits: Default::default(),
        searched_queries: Default::default(),
        search_hits: std::sync::Mutex::new(vec![
            external_entry(Some("tool-capture")),
            external_entry(None),
        ]),
        inspect_external_entry: Default::default(),
        inspect_summaries: Default::default(),
        search_error: Default::default(),
        fetched: Default::default(),
    });
    let kernel = test_kernel(
        engine.clone(),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        None,
    );
    let output = kernel
        .resolve_engine_query(
            ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            EngineQuery::SearchExternal {
                query: "x".into(),
                kind: None,
                scope: None,
                task_id: None,
                label: None,
                limit: 10,
            },
        )
        .await;
    assert!(
        output.model_content.contains("source=tool-capture"),
        "a hit with a known source must render it: {}",
        output.model_content
    );
    assert!(
        output.model_content.contains("residency="),
        "hits must name residency as data: {}",
        output.model_content
    );
    assert!(
        !output.model_content.contains("next:"),
        "search hits must not include an instruction line: {}",
        output.model_content
    );
    assert!(
        output.model_content.contains("source=-"),
        "a hit without a source must render the dash placeholder"
    );
    assert_eq!(output.metadata["op"], "search");
    assert_eq!(output.metadata["kind"], "context");
    assert_eq!(
        output.metadata["descriptors"].as_array().map(|a| a.len()),
        Some(2),
        "search hits must carry bounded ResourceDescriptors"
    );
}

#[tokio::test]
async fn inspect_renders_the_source_authority() {
    // inspect 元数据视图与 residency/semantic 并列展示来源权威。
    let engine = Arc::new(RecordingEngine {
        searched_limits: Default::default(),
        searched_queries: Default::default(),
        search_hits: Default::default(),
        inspect_external_entry: std::sync::Mutex::new(Some(external_entry(Some("tool-session")))),
        inspect_summaries: Default::default(),
        search_error: Default::default(),
        fetched: Default::default(),
    });
    let kernel = test_kernel(
        engine.clone(),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        None,
    );
    let output = kernel
        .resolve_engine_query(
            ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            EngineQuery::InspectExternal {
                item_id: ContextItemId::new(),
            },
        )
        .await;
    assert!(
        output.model_content.contains("source=tool-session"),
        "inspect must render the source authority: {}",
        output.model_content
    );
    assert_eq!(output.metadata["op"], "inspect");
    assert_eq!(output.metadata["kind"], "context");
}

fn placeholder_output() -> ToolOutput {
    ToolOutput {
        call_id: "c1".into(),
        tool_name: "context.manage".into(),
        ok: true,
        summary: "placeholder".into(),
        model_content: "placeholder".into(),
        artifact_ref: None,
        metadata: serde_json::Value::Null,
    }
}

fn query_kernel(engine: Arc<RecordingEngine>) -> Arc<CoreAuthority> {
    test_kernel(
        engine,
        Arc::new(BigOutputDispatcher {
            output: placeholder_output(),
        }),
        None,
    )
}

fn dead_summary(id: ContextItemId) -> ContextItemSummary {
    ContextItemSummary {
        id,
        kind: ContextKind::Note,
        scope: ContextScope::Task,
        scope_id: None,
        attention: AttentionState::Archived,
        semantic: SemanticState::Tombstoned,
        importance: 0.0,
        relevance: 0.0,
        created_tick: 0,
        created_turn: 0,
        last_access_turn: 0,
        last_selected_turn: 0,
        access_count: 0,
        dependencies: Vec::new(),
        keep_alive: false,
        lease_until_turn: None,
        source: None,
    }
}

#[tokio::test]
async fn inspect_unknown_is_not_found() {
    let kernel = query_kernel(Arc::new(RecordingEngine::default()));
    let output = kernel
        .resolve_engine_query(
            placeholder_output(),
            EngineQuery::InspectExternal {
                item_id: ContextItemId::new(),
            },
        )
        .await;
    assert!(output.ok);
    assert_eq!(output.metadata["miss"], "not_found");
}

#[tokio::test]
async fn inspect_terminal_item_is_evidence_absent() {
    let item_id = ContextItemId::new();
    let kernel = query_kernel(Arc::new(RecordingEngine {
        inspect_summaries: std::sync::Mutex::new(vec![dead_summary(item_id)]),
        ..Default::default()
    }));
    let output = kernel
        .resolve_engine_query(
            placeholder_output(),
            EngineQuery::InspectExternal { item_id },
        )
        .await;
    assert!(output.ok);
    assert_eq!(output.metadata["miss"], "evidence_absent");
    assert!(
        output.model_content.contains("not current evidence"),
        "{}",
        output.model_content
    );
}

#[tokio::test]
async fn search_provider_error_is_unavailable() {
    let kernel = query_kernel(Arc::new(RecordingEngine {
        search_error: std::sync::Mutex::new(Some("index offline".into())),
        ..Default::default()
    }));
    let output = kernel
        .resolve_engine_query(
            placeholder_output(),
            EngineQuery::SearchExternal {
                query: "x".into(),
                kind: None,
                scope: None,
                task_id: None,
                label: None,
                limit: 10,
            },
        )
        .await;
    assert!(!output.ok);
    assert_eq!(output.metadata["miss"], "provider_unavailable");
}

#[tokio::test]
async fn fetch_renders_the_source_authority() {
    // fetch 的头部行携带来源权威，正文仍走有界输出。
    let mut item = test_item("stored body".into());
    item.source = Some("tool-capture".into());
    let engine = Arc::new(RecordingEngine {
        searched_limits: Default::default(),
        searched_queries: Default::default(),
        search_hits: Default::default(),
        inspect_external_entry: Default::default(),
        inspect_summaries: Default::default(),
        search_error: Default::default(),
        fetched: std::sync::Mutex::new(Some(item)),
    });
    let kernel = test_kernel(
        engine.clone(),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        None,
    );
    let output = kernel
        .resolve_engine_query(
            ToolOutput {
                call_id: "c1".into(),
                tool_name: "context.manage".into(),
                ok: true,
                summary: "placeholder".into(),
                model_content: "placeholder".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
            EngineQuery::FetchExternal {
                item_id: ContextItemId::new(),
            },
        )
        .await;
    assert!(
        output.model_content.contains("source=tool-capture"),
        "fetch must render the source authority in the header: {}",
        output.model_content
    );
    assert!(output.model_content.contains("stored body"));
}

#[tokio::test]
async fn fetch_of_resident_names_catalog_not_working_set() {
    let mut entry = external_entry(None);
    entry.residency = ContextResidency::Resident;
    let kernel = query_kernel(Arc::new(RecordingEngine {
        inspect_external_entry: std::sync::Mutex::new(Some(entry)),
        ..Default::default()
    }));
    let output = kernel
        .resolve_engine_query(
            placeholder_output(),
            EngineQuery::FetchExternal {
                item_id: ContextItemId::new(),
            },
        )
        .await;
    assert!(output.ok);
    assert_eq!(output.summary, "item in catalog, not stored");
    assert!(
        output.model_content.contains("lives in the catalog")
            && output
                .model_content
                .contains("not the selected working set"),
        "a Resident fetch names the catalog location, not the working set: {}",
        output.model_content
    );
    assert!(
        !output.model_content.contains("already in the working set"),
        "must not claim the body is already in the prompt: {}",
        output.model_content
    );
}

// --- ACI v2 shadow mode (IntentShadowGate) ---

/// A deterministic shadow gate for the kernel integration test.
struct FixedShadowGate(agent_contracts::ShadowVerdict);

#[async_trait::async_trait]
impl agent_contracts::IntentShadowGate for FixedShadowGate {
    async fn shadow_verdict(
        &self,
        _call: &ToolCall,
        _spec: &ToolSpec,
    ) -> agent_contracts::ShadowVerdict {
        self.0.clone()
    }
}

#[tokio::test]
async fn execute_tool_publishes_the_shadow_decision_event() {
    let shadow = Arc::new(FixedShadowGate(agent_contracts::ShadowVerdict::Denied {
        reason: "no live standing grant matches the derived intent (workspace write to 'x')".into(),
    }));
    let kernel = Arc::new(CoreAuthority::new(
        CoreAuthorityConfig {
            shadow_gate: Some(shadow),
            ..CoreAuthorityConfig::default()
        },
        Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
            inspect_summaries: Default::default(),
            search_error: Default::default(),
            fetched: Default::default(),
        }),
        Arc::new(BigOutputDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "big.tool".into(),
                ok: true,
                summary: "done".into(),
                model_content: "ok".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        Arc::new(AllowAllApproval),
        None,
        None,
    ));
    let mut events = kernel.event_sender().subscribe();

    let tool_call = call("big.tool");
    let generation = kernel.current_authority_epoch();
    let execution = kernel
        .execute_tool(
            operation_identity(&kernel, &tool_call, generation),
            tool_call,
            CancellationToken::new(),
            &surface_with("big.tool"),
            generation,
        )
        .await;
    let crate::port::CoreToolExecution { outcome, lease, .. } = execution;
    assert!(
        lease.is_none(),
        "a read-only call carries no commit-time lease"
    );
    assert!(
        matches!(outcome, ToolOutcome::Value(_)),
        "the legacy gate still runs and the call executes"
    );

    // Admission publication now precedes every execution lifecycle row;
    // scan the bounded prefix for the shadow comparison under test.
    let mut decision = None;
    for _ in 0..4 {
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("a ShadowDecision event must be published")
            .expect("stream open");
        if let RuntimeEvent::ShadowDecision {
            call_name,
            legacy_allowed,
            shadow,
        } = envelope.event
        {
            decision = Some((call_name, legacy_allowed, shadow));
            break;
        }
    }
    let (call_name, legacy_allowed, shadow) =
        decision.expect("the bounded execution prefix must contain ShadowDecision");
    assert_eq!(call_name, "big.tool");
    assert!(legacy_allowed, "the legacy AllowAll gate allowed the call");
    assert!(
        matches!(shadow, agent_contracts::ShadowVerdict::Denied { .. }),
        "the shadow gate recorded its v2 refusal"
    );
}

/// Read the next `LeaseIssued` audit row, skipping any events published
/// before it (the shadow comparison lands first).
async fn next_lease_issued(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
) -> agent_contracts::RuntimeEvent {
    for _ in 0..4 {
        let envelope = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("a lease audit event must be published")
            .expect("stream open");
        if let agent_contracts::RuntimeEvent::LeaseIssued { .. } = envelope.event {
            return envelope.event;
        }
    }
    panic!("no LeaseIssued event published");
}

#[tokio::test]
async fn execute_tool_mints_a_commit_time_lease_for_side_effecting_calls() {
    let shadow = Arc::new(FixedShadowGate(agent_contracts::ShadowVerdict::Granted {
        grant_id: "g-1".into(),
        reason: "workspace write inside grant g-1".into(),
    }));
    let kernel = Arc::new(CoreAuthority::new(
        CoreAuthorityConfig {
            shadow_gate: Some(shadow),
            lease_ttl_ms: Some(5_000),
            ..CoreAuthorityConfig::default()
        },
        Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
            inspect_summaries: Default::default(),
            search_error: Default::default(),
            fetched: Default::default(),
        }),
        Arc::new(EchoDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "fs.write".into(),
                ok: true,
                summary: "done".into(),
                model_content: "ok".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        Arc::new(AllowAllApproval),
        None,
        None,
    ));
    let mut events = kernel.event_sender().subscribe();

    let surface = ToolSurfaceSnapshot {
        specs: vec![ToolSpec {
            name: "fs.write".into(),
            description: "write".into(),
            input_schema: serde_json::json!({}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: vec![ToolSemanticRole::Mutate],
        }],
        ..ToolSurfaceSnapshot::default()
    };
    let write_call = ToolCall {
        id: "c1".into(),
        name: "fs.write".into(),
        arguments: serde_json::json!({"path": "src/main.rs", "content": "fn main() {}"}),
    };
    let generation = kernel.current_authority_epoch();
    let execution = kernel
        .execute_tool(
            operation_identity(&kernel, &write_call, generation),
            write_call,
            CancellationToken::new(),
            &surface,
            generation,
        )
        .await;
    let crate::port::CoreToolExecution { outcome, lease, .. } = execution;
    assert!(matches!(outcome, ToolOutcome::Value(_)));
    let lease = lease.expect("a side-effecting call must mint a lease");
    assert_eq!(
        lease.operation_generation, generation,
        "the lease is bound to the operation generation"
    );
    assert_eq!(
        lease.grant_id.as_deref(),
        Some("g-1"),
        "the covering grant from the v2 shadow verdict is recorded"
    );
    assert_eq!(
        lease.intent,
        agent_contracts::EffectIntent::WorkspaceWrite {
            path: "src/main.rs".into(),
            content_bytes: "fn main() {}".len() as u64,
        }
    );
    let now = now_ms();
    assert!(
        lease.issued_at_ms <= now && now <= lease.expires_at_ms,
        "the lease window contains the present instant"
    );
    assert_eq!(
        lease.expires_at_ms - lease.issued_at_ms,
        5_000,
        "the configured TTL bounds the lease window"
    );

    // The bounded audit row is published beside the shadow comparison.
    let RuntimeEvent::LeaseIssued {
        lease_id,
        call_name,
        grant_id,
        expires_at_ms,
    } = next_lease_issued(&mut events).await
    else {
        panic!("expected LeaseIssued");
    };
    assert_eq!(lease_id, lease.lease_id);
    assert_eq!(call_name, "fs.write");
    assert_eq!(grant_id.as_deref(), Some("g-1"));
    assert_eq!(expires_at_ms, lease.expires_at_ms);
}

#[tokio::test]
async fn execute_tool_rejects_a_stale_authority_epoch_before_dispatch() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    struct CountingTools(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl ToolDispatcher for CountingTools {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "count.tool".into(),
                description: "count dispatches".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            }]
        }

        async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutcome::Value(ToolOutput {
                call_id: request.call.id,
                tool_name: request.call.name,
                ok: true,
                summary: "executed".into(),
                model_content: "executed".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            }))
        }
    }

    let kernel = CoreAuthority::new(
        CoreAuthorityConfig::default(),
        Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
            inspect_summaries: Default::default(),
            search_error: Default::default(),
            fetched: Default::default(),
        }),
        Arc::new(CountingTools(executions.clone())),
        Arc::new(AllowAllApproval),
        None,
        None,
    );
    let stale = kernel.current_authority_epoch();
    kernel.advance_authority_epoch(stale).unwrap();
    let surface = ToolSurfaceSnapshot {
        specs: kernel.tools.specs(),
        ..ToolSurfaceSnapshot::default()
    };
    let tool_call = ToolCall {
        id: "stale-call".into(),
        name: "count.tool".into(),
        arguments: serde_json::json!({}),
    };
    let execution = kernel
        .execute_tool(
            operation_identity(&kernel, &tool_call, stale),
            tool_call,
            CancellationToken::new(),
            &surface,
            stale,
        )
        .await;
    let crate::port::CoreToolExecution { outcome, lease, .. } = execution;
    assert!(lease.is_none());
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
    let ToolOutcome::Value(output) = outcome else {
        panic!("stale dispatch must return a bounded value error")
    };
    assert!(!output.ok);
    assert!(output.model_content.contains("authority fence"));
}

#[tokio::test]
async fn execute_tool_rechecks_epoch_after_awaiting_approval() {
    struct BlockingApproval {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait::async_trait]
    impl ApprovalGate for BlockingApproval {
        async fn authorize(
            &self,
            _call: &ToolCall,
            _spec: &ToolSpec,
            _cancel: &CancellationToken,
        ) -> AgentResult<ApprovalDecision> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(ApprovalDecision::Allow)
        }
    }

    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    struct CountingTools(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl ToolDispatcher for CountingTools {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "count.tool".into(),
                description: "count dispatches".into(),
                input_schema: serde_json::json!({}),
                risk: ToolRisk::ReadOnly,
                output_budget: None,
                roles: Vec::new(),
            }]
        }
        async fn execute(&self, request: ToolExecutionRequest) -> AgentResult<ToolOutcome> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutcome::Value(tool_error_output(
                &request.call,
                "executed".into(),
            )))
        }
    }

    let kernel = Arc::new(CoreAuthority::new(
        CoreAuthorityConfig::default(),
        Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
            inspect_summaries: Default::default(),
            search_error: Default::default(),
            fetched: Default::default(),
        }),
        Arc::new(CountingTools(executions.clone())),
        Arc::new(BlockingApproval {
            entered: entered.clone(),
            release: release.clone(),
        }),
        None,
        None,
    ));
    let generation = kernel.current_authority_epoch();
    let surface = ToolSurfaceSnapshot {
        specs: kernel.tools.specs(),
        ..ToolSurfaceSnapshot::default()
    };
    let running = {
        let kernel = kernel.clone();
        tokio::spawn(async move {
            let tool_call = ToolCall {
                id: "approval-race".into(),
                name: "count.tool".into(),
                arguments: serde_json::json!({}),
            };
            kernel
                .execute_tool(
                    operation_identity(&kernel, &tool_call, generation),
                    tool_call,
                    CancellationToken::new(),
                    &surface,
                    generation,
                )
                .await
        })
    };
    entered.notified().await;
    kernel.advance_authority_epoch(generation).unwrap();
    release.notify_one();
    let crate::port::CoreToolExecution { outcome, lease, .. } = running.await.unwrap();
    assert!(lease.is_none());
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
    let ToolOutcome::Value(output) = outcome else {
        panic!("stale post-approval dispatch must be a value error")
    };
    assert!(output.model_content.contains("stale"));
}

#[tokio::test]
async fn lease_is_minted_even_when_the_shadow_gate_denies() {
    // Shadow is observational: the legacy gate allowed the call, so it
    // executes and mints a lease. The lease records that no v2 grant
    // covered the intent — the audit truth, not an enforcement stop.
    let shadow = Arc::new(FixedShadowGate(agent_contracts::ShadowVerdict::Denied {
        reason: "no live standing grant matches the derived intent".into(),
    }));
    let kernel = Arc::new(CoreAuthority::new(
        CoreAuthorityConfig {
            shadow_gate: Some(shadow),
            ..CoreAuthorityConfig::default()
        },
        Arc::new(RecordingEngine {
            searched_limits: Default::default(),
            searched_queries: Default::default(),
            search_hits: Default::default(),
            inspect_external_entry: Default::default(),
            inspect_summaries: Default::default(),
            search_error: Default::default(),
            fetched: Default::default(),
        }),
        Arc::new(EchoDispatcher {
            output: ToolOutput {
                call_id: "c1".into(),
                tool_name: "shell.exec".into(),
                ok: true,
                summary: "done".into(),
                model_content: "ok".into(),
                artifact_ref: None,
                metadata: serde_json::Value::Null,
            },
        }),
        Arc::new(AllowAllApproval),
        None,
        None,
    ));

    let surface = ToolSurfaceSnapshot {
        specs: vec![ToolSpec {
            name: "shell.exec".into(),
            description: "run".into(),
            input_schema: serde_json::json!({}),
            risk: ToolRisk::ProcessExecution,
            output_budget: None,
            roles: vec![ToolSemanticRole::EscapeHatch],
        }],
        ..ToolSurfaceSnapshot::default()
    };
    let tool_call = ToolCall {
        id: "c1".into(),
        name: "shell.exec".into(),
        arguments: serde_json::json!({"command": "cargo test"}),
    };
    let generation = kernel.current_authority_epoch();
    let execution = kernel
        .execute_tool(
            operation_identity(&kernel, &tool_call, generation),
            tool_call,
            CancellationToken::new(),
            &surface,
            generation,
        )
        .await;
    let crate::port::CoreToolExecution { lease, .. } = execution;
    let lease = lease.expect("a side-effecting call mints a lease");
    assert_eq!(
        lease.grant_id, None,
        "a shadow-denied call records that no v2 grant covered it"
    );
    assert_eq!(
        lease.intent,
        agent_contracts::EffectIntent::ProcessRun {
            command: "cargo test".into()
        }
    );
}
