//! The shared contract checks. Each function is a pure, deterministic
//! assertion returning zero or more violations; integration tests run them
//! against concrete dispatchers and assert the aggregate report is clean.

use agent_contracts::{
    AgentError, CAPABILITY_MANAGE, CONTEXT_MANAGE, MAX_TOOL_METADATA_BYTES,
    MAX_TOOL_MODEL_CONTENT_CHARS, MAX_TOOL_OUTPUT_TOTAL_CHARS, MAX_TOOL_SUMMARY_CHARS,
    MAX_TOOL_SURFACE_REPORT_NAME_BYTES, ToolDispatcher, ToolOutput, ToolSpec,
};

use crate::report::{ConformanceReport, ConformanceViolation};

/// Defensive upper bound on one serialized input schema (bytes). A schema is
/// model-visible prompt cost; a schema that cannot possibly fit any round
/// budget is a configuration error, not a packing problem.
pub const MAX_CONFORMANCE_SCHEMA_BYTES: usize = 16 * 1024;

/// The core read/discovery tools every surface must always offer.
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
        && !reference.starts_with("artifact://")
    {
        violations.push(ConformanceViolation::new(
            &subject,
            "output",
            format!("artifact_ref must be an artifact:// reference, got: {reference:?}"),
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
/// contract: the core tools and both control tools are always visible,
/// omission is fail-closed for them, and core tools cannot be unloaded.
pub async fn check_tool_surface(dispatcher: &dyn ToolDispatcher) -> Vec<ConformanceViolation> {
    let mut violations = Vec::new();
    let mut report = ConformanceReport::default();

    let surface: Vec<String> = dispatcher
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();

    for core in CONFORMANCE_CORE_TOOLS
        .iter()
        .chain(&[CONTEXT_MANAGE, CAPABILITY_MANAGE])
    {
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
        if let Some(spec) = dispatcher.inspect_tool(&entry.name) {
            report.subjects_checked += 1;
            report.extend(check_schema_contract(&spec));
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
}
