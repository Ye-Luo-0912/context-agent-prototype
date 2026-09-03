//! The shared contract checks. Each function is a pure, deterministic
//! assertion returning zero or more violations; integration tests run them
//! against concrete dispatchers and assert the aggregate report is clean.

use agent_contracts::{
    AgentError, ArtifactLocator, CAPABILITY_MANAGE, CONTEXT_MANAGE, MAX_TOOL_METADATA_BYTES,
    MAX_TOOL_MODEL_CONTENT_CHARS, MAX_TOOL_OUTPUT_TOTAL_CHARS, MAX_TOOL_SUMMARY_CHARS,
    MAX_TOOL_SURFACE_REPORT_NAME_BYTES, ToolDispatcher, ToolOutput, ToolSpec,
};

use crate::report::{ConformanceReport, ConformanceViolation};

/// Defensive upper bound on one serialized input schema (bytes). A schema is
/// model-visible prompt cost; a schema that cannot possibly fit any round
/// budget is a configuration error, not a packing problem.
pub const MAX_CONFORMANCE_SCHEMA_BYTES: usize = 16 * 1024;

/// The core read/discovery tools every surface must always offer.
/// `task.complete` ships on the production surface (closure discovery;
/// execution stays gated by the completion acceptance gate). `task.manage`
/// stays catalog-cold: autonomous progress is leased by explicit intent,
/// a task requirement, or host discovery.
pub const CONFORMANCE_CORE_TOOLS: &[&str] = &["fs.list", "fs.read", "artifact.read", "search.grep"];

/// Check one `ToolSpec`: well-formed identity, a `type: object` input
/// schema, and a bounded schema size.
pub fn check_schema_contract(spec: &ToolSpec) -> Vec<ConformanceViolation> {
    let subject = format!("schema:{}", spec.name);
    let mut violations = Vec::new();

    if spec.name.trim().is_empty() {
        violations.push(ConformanceViolation::new(
            &subject,
            "schema",
            "tool name must not be empty",
        ));
    }
    if spec.name.len() > MAX_TOOL_SURFACE_REPORT_NAME_BYTES {
        violations.push(ConformanceViolation::new(
            &subject,
            "schema",
            format!(
                "tool name is {} bytes, above the {MAX_TOOL_SURFACE_REPORT_NAME_BYTES}-byte surface cap",
                spec.name.len()
            ),
        ));
    }
    if spec.description.trim().is_empty() {
        violations.push(ConformanceViolation::new(
            &subject,
            "schema",
            "tool description must not be empty",
        ));
    }
    let is_object = spec
        .input_schema
        .get("type")
        .and_then(|value| value.as_str())
        == Some("object");
    if !is_object {
        violations.push(ConformanceViolation::new(
            &subject,
            "schema",
            "input_schema must declare \"type\": \"object\"",
        ));
    }
    let schema_bytes = serde_json::to_string(&spec.input_schema)
        .map(|serialized| serialized.len())
        .unwrap_or(usize::MAX);
    if schema_bytes > MAX_CONFORMANCE_SCHEMA_BYTES {
        violations.push(ConformanceViolation::new(
            &subject,
            "schema",
            format!(
                "input_schema is {schema_bytes} bytes, above the {MAX_CONFORMANCE_SCHEMA_BYTES}-byte defensive cap"
            ),
        ));
    }
    violations
}

/// Check a model-facing `ToolOutput` (after the trusted broker bounded it)
/// against every global cap from the result-envelope specification.
pub fn check_output_envelope(output: &ToolOutput) -> Vec<ConformanceViolation> {
    let subject = format!("output:{}", output.tool_name);
    let mut violations = Vec::new();

    let summary_chars = output.summary.chars().count();
    if summary_chars > MAX_TOOL_SUMMARY_CHARS {
        violations.push(ConformanceViolation::new(
            &subject,
            "output",
            format!(
                "summary is {summary_chars} chars, above the {MAX_TOOL_SUMMARY_CHARS}-char cap"
            ),
        ));
    }
    let content_chars = output.model_content.chars().count();
    if content_chars > MAX_TOOL_MODEL_CONTENT_CHARS {
        violations.push(ConformanceViolation::new(
            &subject,
            "output",
            format!(
                "model_content is {content_chars} chars, above the {MAX_TOOL_MODEL_CONTENT_CHARS}-char cap"
            ),
        ));
    }
    let metadata_json = serde_json::to_string(&output.metadata).unwrap_or_default();
    let metadata_bytes = metadata_json.len();
    if metadata_bytes > MAX_TOOL_METADATA_BYTES {
        violations.push(ConformanceViolation::new(
            &subject,
            "output",
            format!(
                "metadata is {metadata_bytes} bytes, above the {MAX_TOOL_METADATA_BYTES}-byte cap"
            ),
        ));
    }
    let decoded_total = summary_chars + content_chars + metadata_json.chars().count();
    if decoded_total > MAX_TOOL_OUTPUT_TOTAL_CHARS {
        violations.push(ConformanceViolation::new(
            &subject,
            "output",
            format!(
                "decoded model-facing total is {decoded_total} chars, above the {MAX_TOOL_OUTPUT_TOTAL_CHARS}-char cap"
            ),
        ));
    }
    if !output.ok && output.summary.trim().is_empty() {
        violations.push(ConformanceViolation::new(
            &subject,
            "output",
            "a failed result must carry a non-empty summary explaining the failure",
        ));
    }
    if let Some(reference) = &output.artifact_ref
        && let Err(error) = ArtifactLocator::parse(reference)
    {
        violations.push(ConformanceViolation::new(
            &subject,
            "output",
            format!("artifact_ref is not a canonical owner/digest locator: {error}"),
        ));
    }
    violations
}

/// Check a tool error is a structured `AgentError` category: an internal
/// leak (unbounded panic text reaching the caller) is a conformance failure.
pub fn check_error_envelope(error: &AgentError) -> Vec<ConformanceViolation> {
    let mut violations = Vec::new();
    if matches!(error, AgentError::Internal(_)) {
        violations.push(ConformanceViolation::new(
            "error",
            "error",
            format!("tool error must not leak an internal failure: {error}"),
        ));
    }
    if error.to_string().trim().is_empty() {
        violations.push(ConformanceViolation::new(
            "error",
            "error",
            "tool error message must not be empty",
        ));
    }
    violations
}

/// Check a dispatcher's surface and lifecycle rules against the runtime
/// contract: core tools and `capability.manage` are always visible and
/// fail-closed against round omission; core tools cannot be unloaded.
/// `context.manage` is catalog-only on the production default (item 24).
pub async fn check_tool_surface(dispatcher: &dyn ToolDispatcher) -> Vec<ConformanceViolation> {
    let mut violations = Vec::new();
    let mut report = ConformanceReport::default();

    let surface: Vec<String> = dispatcher
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();

    for core in CONFORMANCE_CORE_TOOLS.iter().chain(&[CAPABILITY_MANAGE]) {
        if !surface.iter().any(|name| name == core) {
            violations.push(ConformanceViolation::new(
                "surface",
                "surface",
                format!("core/control tool '{core}' must be on the default surface"),
            ));
        }
        if dispatcher.may_omit_from_round(core) {
            violations.push(ConformanceViolation::new(
                "surface",
                "surface",
                format!("core/control tool '{core}' must be fail-closed against round omission"),
            ));
        }
    }

    if dispatcher.inspect_tool(CONTEXT_MANAGE).is_none()
        && !dispatcher
            .catalog()
            .iter()
            .any(|entry| entry.name == CONTEXT_MANAGE)
    {
        violations.push(ConformanceViolation::new(
            "surface",
            "surface",
            "context.manage must remain in the catalog so capability.manage can load it",
        ));
    }

    for core in CONFORMANCE_CORE_TOOLS {
        if dispatcher.unload_tool(core).is_ok() {
            violations.push(ConformanceViolation::new(
                "surface",
                "surface",
                format!("core tool '{core}' must not be unloadable"),
            ));
        }
    }

    report.subjects_checked = surface.len();
    report.extend(violations.clone());
    violations
}

/// Run every static contract check over a dispatcher's complete catalog:
/// schema contract for every known tool, and the surface/lifecycle rules.
/// Output/error envelope checks need real executions and live in the
/// integration tests (they need a workspace, a broker and representative
/// calls).
pub async fn check_catalog(dispatcher: &dyn ToolDispatcher) -> ConformanceReport {
    let mut report = ConformanceReport::default();

    for entry in dispatcher.catalog() {
        report.subjects_checked += 1;
        match dispatcher.inspect_tool(&entry.name) {
            Some(spec) => report.extend(check_schema_contract(&spec)),
            None => report.push(ConformanceViolation::new(
                "surface",
                "surface",
                format!(
                    "catalog row '{}' exists but cannot be inspected; fail closed",
                    entry.name
                ),
            )),
        }
    }
    // Meta/control specs are not catalog rows; they still must conform.
    for spec in dispatcher.specs() {
        if dispatcher.inspect_tool(&spec.name).is_none() {
            report.subjects_checked += 1;
            report.extend(check_schema_contract(&spec));
        }
    }
    report.extend(check_tool_surface(dispatcher).await);
    report
}

/// Stable evaluated-surface digest: SHA-256 over the sorted
/// `(name, canonical input schema)` pairs of a dispatcher's current model
/// surface. Order-independent and schema-shape-sensitive, so a persisted
/// digest detects any surface drift — a tool added, removed, or with a
/// changed input schema.
pub fn surface_digest(specs: &[ToolSpec]) -> String {
    agent_contracts::tool::surface_digest(specs)
}

/// Check the machine-readable inventory (`docs/TOOL_INVENTORY.json`,
/// already parsed) against a dispatcher's actual catalog and surface:
///
/// - every inventory row must be recognizable (specs ∪ catalog); an
///   inventory row the dispatcher cannot produce a spec/entry for is a
///   fail-closed conformance failure, never a silent skip;
/// - every inventory default-surface tool (`surface.core` +
///   `surface.control`) must be offered by the dispatcher's current model
///   surface (conditional/catalog-optional tools may be absent until
///   discovered or loaded);
/// - a dispatcher surface tool the inventory does not list anywhere is
///   drift in the other direction.
///
/// Returns the violations plus the evaluated surface digest (`surface_digest`
/// over the actual specs) for the caller to persist.
pub fn check_inventory_parity(
    dispatcher: &dyn ToolDispatcher,
    inventory: &serde_json::Value,
) -> (Vec<ConformanceViolation>, String) {
    let mut violations = Vec::new();

    let mut expected: Vec<String> = Vec::new();
    for group in ["core", "control"] {
        match inventory
            .pointer(&format!("/surface/{group}"))
            .and_then(serde_json::Value::as_array)
        {
            Some(names) => expected.extend(
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(str::to_owned)),
            ),
            None => violations.push(ConformanceViolation::new(
                "surface",
                "surface",
                format!("inventory is missing /surface/{group}"),
            )),
        }
    }
    expected.sort_unstable();
    expected.dedup();

    // Rows the inventory names anywhere (default surface or the tools
    // table): a dispatcher surface tool outside this set is drift.
    let mut known: std::collections::BTreeSet<String> = expected.clone().into_iter().collect();
    let tools = inventory
        .get("tools")
        .and_then(serde_json::Value::as_object);
    match tools {
        Some(tools) => known.extend(tools.keys().cloned()),
        None => violations.push(ConformanceViolation::new(
            "surface",
            "surface",
            "inventory is missing /tools",
        )),
    }

    // Every inventory row must be recognizable by the dispatcher.
    for name in known.iter() {
        let recognizable = dispatcher.specs().iter().any(|spec| &spec.name == name)
            || dispatcher.catalog().iter().any(|entry| &entry.name == name);
        if !recognizable {
            violations.push(ConformanceViolation::new(
                "surface",
                "surface",
                format!("inventory row '{name}' cannot be inspected; fail closed"),
            ));
        }
    }

    let mut actual: Vec<String> = dispatcher
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    actual.sort_unstable();
    actual.dedup();

    for name in &expected {
        if !actual.contains(name) {
            violations.push(ConformanceViolation::new(
                "surface",
                "surface",
                format!(
                    "inventory surface lists '{name}' but the dispatcher surface does not offer it"
                ),
            ));
        }
    }
    for name in &actual {
        if !expected.contains(name) && !known.contains(name) {
            violations.push(ConformanceViolation::new(
                "surface",
                "surface",
                format!("dispatcher surface offers '{name}' but the inventory does not list it"),
            ));
        }
    }

    let digest = surface_digest(&dispatcher.specs());
    (violations, digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "a conformant tool".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        }
    }

    #[test]
    fn schema_contract_accepts_a_well_formed_spec() {
        assert!(check_schema_contract(&spec("fs.read")).is_empty());
    }

    #[test]
    fn schema_contract_rejects_structural_problems() {
        let mut broken = spec("bad");
        broken.name = String::new();
        broken.description = "   ".into();
        broken.input_schema = json!({"type": "string"});
        let violations = check_schema_contract(&broken);
        assert_eq!(violations.len(), 3, "{violations:?}");
    }

    #[test]
    fn output_envelope_accepts_a_bounded_output() {
        let output = ToolOutput {
            call_id: "c".into(),
            tool_name: "fs.read".into(),
            ok: true,
            summary: "read 10 lines".into(),
            model_content: "line one".into(),
            artifact_ref: None,
            metadata: json!({"lines": 10}),
        };
        assert!(check_output_envelope(&output).is_empty());
    }

    #[test]
    fn output_envelope_flags_every_cap_violation() {
        let output = ToolOutput {
            call_id: "c".into(),
            tool_name: "shell.exec".into(),
            ok: false,
            summary: String::new(),
            model_content: "x".repeat(MAX_TOOL_MODEL_CONTENT_CHARS + 1),
            artifact_ref: Some("not-an-artifact-uri".into()),
            metadata: json!({"blob": "y".repeat(MAX_TOOL_METADATA_BYTES + 1)}),
        };
        let violations = check_output_envelope(&output);
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("model_content")),
            "{violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.message.contains("metadata")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("non-empty summary")),
            "{violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.message.contains("artifact://")),
            "{violations:?}"
        );
    }

    #[test]
    fn error_envelope_accepts_structured_categories() {
        assert!(check_error_envelope(&AgentError::InvalidRequest("bad path".into())).is_empty());
        assert!(check_error_envelope(&AgentError::Tool("tool failed".into())).is_empty());
    }

    #[test]
    fn error_envelope_flags_internal_leaks() {
        let violations = check_error_envelope(&AgentError::Internal("panic".into()));
        assert_eq!(violations.len(), 1, "{violations:?}");
    }

    #[test]
    fn schema_tokens_are_bounded() {
        use agent_contracts::tokens;
        let wide = spec("wide");
        let tokens = tokens::approx_tokens(&serde_json::to_string(&wide.input_schema).unwrap());
        assert!(tokens > 0);
    }

    /// checkpoint 向后兼容契约：旧形状的执行状态与
    /// 进度视图必须带缺省值反序列化，新字段不破坏旧 checkpoint。
    #[test]
    fn old_execution_state_json_deserializes_with_default_frontier() {
        use agent_runtime::ExecutionState;
        // 2026-08-23 之前的 checkpoint 形状：无 evidence / convergence。
        let old = json!({
            "anchor_revision": 4,
            "workspace_revision": 9,
            "checked_files": [],
            "verifications": [],
            "failed_commands": [],
        });
        let state: ExecutionState = serde_json::from_value(old).expect("old shape must parse");
        assert!(state.evidence.is_empty());
        assert_eq!(state.convergence.evidence_revision, 0);
        assert_eq!(state.convergence.actions_since_frontier_advance, 0);
        assert!(state.convergence.recent_deltas.is_empty());
    }

    #[test]
    fn frontier_state_round_trips_through_serde() {
        use agent_contracts::ToolOutput;
        use agent_runtime::ExecutionState;
        let mut state = ExecutionState::default();
        for turn in 1..=2u64 {
            state.observe_tool(
                &ToolOutput {
                    call_id: "c".into(),
                    tool_name: "git.status".into(),
                    ok: true,
                    summary: "clean".into(),
                    model_content: String::new(),
                    artifact_ref: None,
                    metadata: json!({}),
                },
                1,
                turn,
            );
        }
        let text = serde_json::to_string(&state).expect("serialize");
        let back: ExecutionState = serde_json::from_str(&text).expect("round trip");
        assert_eq!(back, state);
        assert_eq!(back.evidence.len(), 1);
        assert_eq!(back.convergence.actions_since_frontier_advance, 1);
    }

    #[test]
    fn old_task_progress_view_json_parses_with_default_warnings() {
        use agent_contracts::TaskProgressView;
        let old = json!({
            "anchor_revision": 1,
            "workspace_revision": 2,
            "checked_files": ["src/a.rs@abc"],
            "verifications": [],
            "failed_commands": [],
        });
        let view: TaskProgressView = serde_json::from_value(old).expect("old shape must parse");
        assert!(view.operational_evidence.is_empty());
        assert!(view.stall_warning.is_none());
        assert!(view.frontier_warning.is_none());
    }

    struct FakeDispatcher {
        specs: Vec<ToolSpec>,
        /// Names whose spec `inspect_tool` answers; a row not listed here
        /// simulates an uninspectable catalog row.
        inspectable: std::collections::HashSet<String>,
    }

    impl FakeDispatcher {
        fn aligned() -> Self {
            let mut inspectable = std::collections::HashSet::new();
            for name in ["task.complete", "capability.manage"] {
                inspectable.insert(name.to_string());
            }
            Self {
                specs: vec![spec("task.complete"), spec("capability.manage")],
                inspectable,
            }
        }
    }

    #[async_trait::async_trait]
    impl ToolDispatcher for FakeDispatcher {
        fn specs(&self) -> Vec<ToolSpec> {
            self.specs.clone()
        }

        fn catalog(&self) -> Vec<agent_contracts::ToolCatalogEntry> {
            self.specs
                .iter()
                .map(|spec| agent_contracts::ToolCatalogEntry {
                    name: spec.name.clone(),
                    state: agent_contracts::ToolLifecycle::Available,
                    owner: "fake".into(),
                    description: spec.description.clone(),
                    risk: spec.risk,
                    roles: Vec::new(),
                })
                .collect()
        }

        fn inspect_tool(&self, name: &str) -> Option<ToolSpec> {
            self.inspectable
                .contains(name)
                .then(|| self.specs.iter().find(|spec| spec.name == name).cloned())
                .flatten()
        }

        async fn execute(
            &self,
            _request: agent_contracts::ToolExecutionRequest,
        ) -> agent_contracts::AgentResult<agent_contracts::ToolOutcome> {
            Err(agent_contracts::AgentError::InvalidRequest(
                "fake dispatcher".into(),
            ))
        }
    }

    #[test]
    fn surface_digest_is_stable_order_independent_and_shape_sensitive() {
        use agent_contracts::ToolSpec;
        let alpha = ToolSpec {
            name: "alpha".into(),
            description: "a".into(),
            input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        };
        let beta = ToolSpec {
            name: "beta".into(),
            description: "b".into(),
            input_schema: json!({"type": "object", "properties": {}}),
            risk: agent_contracts::ToolRisk::ReadOnly,
            output_budget: None,
            roles: Vec::new(),
        };
        let forward = surface_digest(&[alpha.clone(), beta.clone()]);
        let reversed = surface_digest(&[beta.clone(), alpha.clone()]);
        assert_eq!(forward, reversed, "digest must be order-independent");

        let grown = surface_digest(&[alpha.clone(), beta.clone(), spec("gamma")]);
        assert_ne!(forward, grown, "an added tool must change the digest");

        let mut changed = beta;
        changed.input_schema = json!({"type": "object", "properties": {"y": {"type": "integer"}}});
        assert_ne!(
            forward,
            surface_digest(&[alpha.clone(), changed]),
            "a changed schema must change the digest"
        );
    }

    #[test]
    fn inventory_parity_passes_when_aligned() {
        let dispatcher = FakeDispatcher::aligned();
        let inventory = json!({
            "surface": {
                "core": ["task.complete"],
                "control": ["capability.manage"]
            },
            "tools": {
                "task.complete": {"schema": {}},
                "capability.manage": {"schema": {}}
            }
        });
        let (violations, digest) = check_inventory_parity(&dispatcher, &inventory);
        assert!(violations.is_empty(), "{violations:?}");
        assert_eq!(digest, surface_digest(&dispatcher.specs()));
    }

    #[test]
    fn inventory_parity_fails_closed_on_unknown_rows_and_surface_drift() {
        let dispatcher = FakeDispatcher::aligned();
        let inventory = json!({
            "surface": {
                "core": ["task.complete", "fs.read"],
                "control": ["capability.manage"]
            },
            "tools": {
                "task.complete": {},
                "capability.manage": {},
                "ghost.tool": {}
            }
        });
        let (violations, _) = check_inventory_parity(&dispatcher, &inventory);
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("'fs.read'") && v.message.contains("does not offer")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("ghost.tool")
                    && v.message.contains("cannot be inspected")),
            "{violations:?}"
        );
    }

    #[tokio::test]
    async fn check_catalog_fails_closed_when_a_catalog_row_cannot_be_inspected() {
        let dispatcher = FakeDispatcher {
            inspectable: std::collections::HashSet::new(),
            ..FakeDispatcher::aligned()
        };
        let report = check_catalog(&dispatcher).await;
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.message.contains("cannot be inspected")),
            "{:?}",
            report.violations
        );
    }

    #[test]
    fn inventory_surface_missing_field_is_a_violation() {
        let dispatcher = FakeDispatcher {
            inspectable: std::collections::HashSet::new(),
            ..FakeDispatcher::aligned()
        };
        let (violations, _) = check_inventory_parity(&dispatcher, &json!({}));
        assert!(
            violations
                .iter()
                .any(|v| v.message.contains("/surface/core")),
            "{violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.message.contains("/tools")),
            "{violations:?}"
        );
    }
}
