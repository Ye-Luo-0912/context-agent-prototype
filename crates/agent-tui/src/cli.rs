//! Headless product entry: one prompt or `--continue`, JSONL events, honest
//! exit codes. Approval never waits for a human. Ungranted writes refuse.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_compose::HostToolPolicyRegistry;
use agent_contracts::{
    ApprovalGate, RuntimeEvent, RuntimeEventEnvelope, RuntimeFailureClass, StandingGrant,
    ToolOutput,
};
use agent_core::{PolicyApprovalGate, TaskApprovalGate};
use agent_runtime::RuntimeHandle;
use tokio::sync::broadcast;
use tokio::time::Instant;

use crate::work::start_long_task;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_ROUND_BUDGET: i32 = 2;
pub const EXIT_APPROVAL_DENIED: i32 = 3;

#[derive(Debug, Clone)]
pub enum HeadlessAction {
    Prompt { text: String, work: bool },
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessOutcome {
    pub exit: i32,
    pub status: &'static str,
    pub stop: String,
    pub task_completed: bool,
    pub round_budget: bool,
    pub approval_denied: bool,
}

/// Headless approval: standing grants auto-allow matching effects; everything
/// else falls through to a read-only policy so a missing human cannot stall
/// or silently allow a write.
pub async fn headless_approval(
    read_only: bool,
    grant_args: &[String],
    host_policies: Arc<HostToolPolicyRegistry>,
) -> anyhow::Result<Arc<dyn ApprovalGate>> {
    if read_only {
        return Ok(Arc::new(PolicyApprovalGate::read_only()) as Arc<dyn ApprovalGate>);
    }
    let gate = Arc::new(
        TaskApprovalGate::new(Arc::new(PolicyApprovalGate::read_only()))
            .with_host_policies(host_policies),
    );
    for json in grant_args {
        let grant: StandingGrant = serde_json::from_str(json)
            .map_err(|error| anyhow::anyhow!("invalid --grant JSON: {json}: {error}"))?;
        gate.grant(grant).await?;
    }
    Ok(gate as Arc<dyn ApprovalGate>)
}

pub fn resolve_prompt(raw: &str) -> anyhow::Result<String> {
    let text = if raw == "-" {
        use std::io::Read;
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .map_err(|error| anyhow::anyhow!("failed to read --prompt=- from stdin: {error}"))?;
        buf
    } else {
        raw.to_string()
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        anyhow::bail!("--prompt is empty");
    }
    Ok(trimmed.to_string())
}

const MAX_GRANT_FILE_BYTES: u64 = 64 * 1024;
const MAX_STANDING_GRANTS: usize = 16;

/// Load standing grants from a JSON object or array. Bounded and fail-closed
/// so an editor task cannot smuggle an unbounded policy blob.
pub fn load_grant_file(path: &Path) -> anyhow::Result<Vec<String>> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| anyhow::anyhow!("unreadable --grant-file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("--grant-file {} is not a regular file", path.display());
    }
    if metadata.len() > MAX_GRANT_FILE_BYTES {
        anyhow::bail!(
            "--grant-file {} is {} bytes; the cap is {MAX_GRANT_FILE_BYTES}",
            path.display(),
            metadata.len()
        );
    }
    let bytes = std::fs::read(path)
        .map_err(|error| anyhow::anyhow!("unreadable --grant-file {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_GRANT_FILE_BYTES {
        anyhow::bail!(
            "--grant-file {} is {} bytes; the cap is {MAX_GRANT_FILE_BYTES}",
            path.display(),
            bytes.len()
        );
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("invalid --grant-file {}: {error}", path.display()))?;
    let items = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(_) => vec![value],
        _ => anyhow::bail!(
            "--grant-file {} must be a JSON object or an array of standing grants",
            path.display()
        ),
    };
    if items.is_empty() {
        anyhow::bail!("--grant-file {} contains no grants", path.display());
    }
    if items.len() > MAX_STANDING_GRANTS {
        anyhow::bail!(
            "--grant-file {} has {} grants; the cap is {MAX_STANDING_GRANTS}",
            path.display(),
            items.len()
        );
    }
    let mut grants = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        serde_json::from_value::<StandingGrant>(item.clone()).map_err(|error| {
            anyhow::anyhow!(
                "invalid standing grant at {index} in --grant-file {}: {error}",
                path.display()
            )
        })?;
        grants.push(serde_json::to_string(&item)?);
    }
    Ok(grants)
}

/// Files first, then `--grant=` so a one-off CLI grant can replace a file
/// entry with the same id. The combined set stays bounded.
pub fn collect_grants(files: &[PathBuf], cli_json: &[String]) -> anyhow::Result<Vec<String>> {
    let mut grants = Vec::new();
    for path in files {
        grants.extend(load_grant_file(path)?);
    }
    grants.extend(cli_json.iter().cloned());
    if grants.len() > MAX_STANDING_GRANTS {
        anyhow::bail!(
            "at most {MAX_STANDING_GRANTS} standing grants (--grant-file and --grant combined)"
        );
    }
    Ok(grants)
}

/// Headless JSONL sink: stdout by default, or a file for editor/task capture.
#[derive(Debug)]
pub enum JsonlWriter {
    Stdout(io::Stdout),
    File(std::fs::File),
}

impl JsonlWriter {
    pub fn open(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            None => Ok(Self::Stdout(io::stdout())),
            Some(path) => {
                let file = std::fs::File::create(path).map_err(|error| {
                    anyhow::anyhow!("cannot create --jsonl-out {}: {error}", path.display())
                })?;
                Ok(Self::File(file))
            }
        }
    }
}

impl Write for JsonlWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Stdout(stdout) => stdout.write(buf),
            Self::File(file) => file.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Stdout(stdout) => stdout.flush(),
            Self::File(file) => file.flush(),
        }
    }
}

pub async fn run_headless(
    handle: RuntimeHandle,
    events: &mut broadcast::Receiver<RuntimeEventEnvelope>,
    action: HeadlessAction,
    timeout: Duration,
    jsonl: &mut impl Write,
) -> anyhow::Result<HeadlessOutcome> {
    match action {
        HeadlessAction::Prompt { text, work: true } => {
            if let Some(warning) = start_long_task(&handle, text).await? {
                eprintln!("{warning}");
            }
        }
        HeadlessAction::Prompt { text, work: false } => {
            handle.user_message(text).await?;
        }
        HeadlessAction::Continue => {
            handle.continue_active_task().await?;
        }
    }

    let mut outcome = Drain {
        turn_completed: false,
        turn_cancelled: false,
        commit_failed: false,
        recovery_required: false,
        task_completed: false,
        task_active: false,
        round_budget: false,
        approval_denied: false,
        other_failure: None,
        timed_out: false,
    };
    let deadline = Instant::now() + timeout;
    loop {
        if outcome.is_terminal() {
            break;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            outcome.timed_out = true;
            break;
        }
        let envelope = tokio::select! {
            received = events.recv() => match received {
                Ok(envelope) => envelope,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    eprintln!(
                        "warning: headless consumer lagged and dropped {skipped} runtime events"
                    );
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tokio::time::sleep(remaining) => {
                outcome.timed_out = true;
                break;
            }
        };
        outcome.observe(&envelope.event);
        if emit_jsonl_event(&envelope) {
            serde_json::to_writer(&mut *jsonl, &envelope)?;
            jsonl.write_all(b"\n")?;
        }
    }

    let task_was_active = outcome.task_active;
    let result = outcome.finish();
    // Closure semantics for scripts: `operator_accepted` = durable
    // TaskCompleted; `awaiting_operator_review` = work produced but the
    // task stays active (/done closes it); `none` = no active task.
    let task_state = if result.task_completed {
        "operator_accepted"
    } else if task_was_active {
        "awaiting_operator_review"
    } else {
        "none"
    };
    let session = serde_json::json!({
        "schema": "agent.headless.v1",
        "kind": "session_end",
        "status": result.status,
        "exit": result.exit,
        "stop": result.stop,
        "task_state": task_state,
        "task_completed": result.task_completed,
        "round_budget": result.round_budget,
        "approval_denied": result.approval_denied,
    });
    serde_json::to_writer(&mut *jsonl, &session)?;
    jsonl.write_all(b"\n")?;
    jsonl.flush()?;
    Ok(result)
}

fn emit_jsonl_event(envelope: &RuntimeEventEnvelope) -> bool {
    // Live-only deltas would drown a script; the durable AssistantMessage
    // and ToolFinished rows remain.
    !matches!(
        envelope.event,
        RuntimeEvent::ModelDelta { .. } | RuntimeEvent::ModelRetrying { .. }
    )
}

fn approval_denied(output: &ToolOutput) -> bool {
    output.summary.contains("denied by approval policy")
        || output.model_content.contains("denied by approval policy")
}

#[derive(Default)]
struct Drain {
    turn_completed: bool,
    turn_cancelled: bool,
    commit_failed: bool,
    recovery_required: bool,
    task_completed: bool,
    task_active: bool,
    round_budget: bool,
    approval_denied: bool,
    other_failure: Option<String>,
    timed_out: bool,
}

impl Drain {
    fn is_terminal(&self) -> bool {
        self.turn_completed
            || self.turn_cancelled
            || self.commit_failed
            || self.recovery_required
            || self.round_budget
            || self.timed_out
    }

    fn observe(&mut self, event: &RuntimeEvent) {
        match event {
            RuntimeEvent::TurnCompleted => self.turn_completed = true,
            RuntimeEvent::TurnCancelled { .. } => self.turn_cancelled = true,
            RuntimeEvent::TurnCommitFailed { .. } => self.commit_failed = true,
            RuntimeEvent::RecoveryRequired => self.recovery_required = true,
            RuntimeEvent::TaskCompleted { .. } => {
                self.task_completed = true;
                self.task_active = false;
            }
            RuntimeEvent::FocusChanged { .. } | RuntimeEvent::TaskAnchorChanged { .. } => {
                self.task_active = true;
            }
            RuntimeEvent::FocusCleared => self.task_active = false,
            RuntimeEvent::Failure {
                class,
                retryable,
                message,
            } => {
                if *class == RuntimeFailureClass::RoundBudget {
                    self.round_budget = true;
                } else if !*retryable {
                    self.other_failure = Some(message.clone());
                }
            }
            RuntimeEvent::ToolFinished { output, .. } if approval_denied(output) => {
                self.approval_denied = true;
            }
            _ => {}
        }
    }

    fn finish(self) -> HeadlessOutcome {
        if self.approval_denied {
            return HeadlessOutcome {
                exit: EXIT_APPROVAL_DENIED,
                status: "approval_denied",
                stop: "approval_denied".into(),
                task_completed: self.task_completed,
                round_budget: self.round_budget,
                approval_denied: true,
            };
        }
        if self.round_budget {
            return HeadlessOutcome {
                exit: EXIT_ROUND_BUDGET,
                status: "round_budget",
                stop: "round_budget".into(),
                task_completed: self.task_completed,
                round_budget: true,
                approval_denied: false,
            };
        }
        if self.timed_out {
            return HeadlessOutcome {
                exit: EXIT_ERROR,
                status: "timeout",
                stop: "timeout".into(),
                task_completed: self.task_completed,
                round_budget: false,
                approval_denied: false,
            };
        }
        if self.turn_cancelled || self.commit_failed || self.recovery_required {
            return HeadlessOutcome {
                exit: EXIT_ERROR,
                status: "failed",
                stop: if self.turn_cancelled {
                    "cancelled"
                } else if self.recovery_required {
                    "recovery_required"
                } else {
                    "commit_failed"
                }
                .into(),
                task_completed: self.task_completed,
                round_budget: false,
                approval_denied: false,
            };
        }
        if self.other_failure.is_some() {
            return HeadlessOutcome {
                exit: EXIT_ERROR,
                status: "failed",
                stop: "failure".into(),
                task_completed: self.task_completed,
                round_budget: false,
                approval_denied: false,
            };
        }
        HeadlessOutcome {
            exit: EXIT_OK,
            status: "completed",
            stop: if self.task_completed {
                "task_completed"
            } else {
                "turn_completed"
            }
            .into(),
            task_completed: self.task_completed,
            round_budget: false,
            approval_denied: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use agent_compose::{
        ComposeConfig, ContextPolicy, HostToolPolicyRegistry, MockModelTransport,
        build_context_engine, compose,
    };
    use agent_contracts::{
        AgentResult, ModelCapabilities, ModelOutput, ModelRequest, ModelTransport, ToolCall,
    };
    use agent_storage::FileEventJournal;
    use agent_workspace::{Workspace, WorkspaceOutputBroker};
    use serde_json::json;
    use tool_runtime::{BuiltinToolDispatcher, VerificationRecipes};

    fn hello_grant() -> String {
        serde_json::json!({
            "id": "hello",
            "risk": "WorkspaceWrite",
            "target": { "workspace_path_prefix": "hello.txt" },
            "constraint": {},
            "expires_at_ms": u64::MAX
        })
        .to_string()
    }

    async fn product_compose(
        root: &std::path::Path,
        grants: &[String],
        model: Arc<dyn ModelTransport>,
        max_tool_rounds: Option<usize>,
    ) -> anyhow::Result<agent_compose::ComposedRuntime> {
        let workspace = Workspace::open(root).await?;
        let journal = Arc::new(FileEventJournal::open(workspace.state_dir().join("traces")).await?);
        let recipes = Arc::new(VerificationRecipes::discover(&workspace)?);
        let host_policies = Arc::new(
            HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
                .map_err(anyhow::Error::msg)?,
        );
        let approval = headless_approval(false, grants, host_policies.clone()).await?;
        let context_engine = build_context_engine(
            ContextPolicy::Dynamic,
            workspace.state_dir(),
            Some(model.clone()),
        )
        .await?;
        let base_tools = Arc::new(BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            Default::default(),
            (*recipes).clone(),
        ));
        let reservation = workspace
            .state_dir()
            .join("authority")
            .join("broker-reservations.jsonl");
        compose(ComposeConfig {
            provider_profile_digest: None,
            defer_proof_refresh: false,
            shadow_context_frame: false,
            workspace: workspace.clone(),
            context_engine,
            model,
            approval,
            base_tools,
            capability_aware: true,
            journal: Some(journal),
            artifact_store: Some(Arc::new(workspace.clone())),
            output_broker: Some(Arc::new(WorkspaceOutputBroker::new(
                workspace.clone().into(),
            ))),
            max_tool_rounds,
            project_task_progress: true,
            project_settlement: false,
            settlement_projection_diagnostics: false,
            project_completion_opportunity: false,
            recovery_surface: false,
            host_policies: Some(host_policies),
            effect_reservation_journal: Some(reservation),
            verification_recipes: Some(recipes.clone()),
            project_proof_refresh: !recipes.is_empty(),
        })
        .await
    }

    fn session_end(jsonl: &str) -> serde_json::Value {
        jsonl
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|value| value.get("kind") == Some(&json!("session_end")))
            .expect("session_end JSONL row")
    }

    #[test]
    fn approval_denied_matches_kernel_refusal_text() {
        let denied = ToolOutput {
            call_id: "1".into(),
            tool_name: "fs.write".into(),
            ok: false,
            summary: "tool denied by approval policy: fs.write".into(),
            model_content: "tool error: tool denied by approval policy: fs.write".into(),
            artifact_ref: None,
            metadata: json!({}),
        };
        assert!(approval_denied(&denied));
        let other = ToolOutput {
            call_id: "2".into(),
            tool_name: "fs.write".into(),
            ok: false,
            summary: "path not found".into(),
            model_content: "missing".into(),
            artifact_ref: None,
            metadata: json!({}),
        };
        assert!(!approval_denied(&other));
    }

    fn hello_grant_value() -> serde_json::Value {
        json!({
            "id": "hello",
            "risk": "WorkspaceWrite",
            "target": { "workspace_path_prefix": "hello.txt" },
            "constraint": {},
            "expires_at_ms": u64::MAX
        })
    }

    #[test]
    fn grant_file_loads_an_object_or_array_and_rejects_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let object = dir.path().join("one.json");
        std::fs::write(&object, hello_grant_value().to_string()).unwrap();
        let loaded = load_grant_file(&object).unwrap();
        assert_eq!(loaded.len(), 1);
        let grant: StandingGrant = serde_json::from_str(&loaded[0]).unwrap();
        assert_eq!(grant.id, "hello");

        let array = dir.path().join("many.json");
        std::fs::write(
            &array,
            serde_json::Value::Array(vec![hello_grant_value()]).to_string(),
        )
        .unwrap();
        assert_eq!(load_grant_file(&array).unwrap().len(), 1);

        let missing = load_grant_file(&dir.path().join("nope.json"))
            .unwrap_err()
            .to_string();
        assert!(missing.contains("unreadable"), "{missing}");

        let garbage = dir.path().join("bad.json");
        std::fs::write(&garbage, "not json").unwrap();
        let invalid = load_grant_file(&garbage).unwrap_err().to_string();
        assert!(invalid.contains("invalid --grant-file"), "{invalid}");

        let empty = dir.path().join("empty.json");
        std::fs::write(&empty, "[]").unwrap();
        let none = load_grant_file(&empty).unwrap_err().to_string();
        assert!(none.contains("no grants"), "{none}");
    }

    #[test]
    fn grant_file_then_cli_grants_stay_bounded_and_cli_appends() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("g.json");
        std::fs::write(&file, hello_grant_value().to_string()).unwrap();
        let combined = collect_grants(&[file], &[hello_grant().replace("hello", "other")]).unwrap();
        assert_eq!(combined.len(), 2);
    }

    #[test]
    fn jsonl_writer_creates_the_editor_capture_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("last.jsonl");
        let mut writer = JsonlWriter::open(Some(&path)).unwrap();
        writer.write_all(b"{\"ok\":true}\n").unwrap();
        writer.flush().unwrap();
        drop(writer);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("ok"), "{body}");

        let missing_parent = dir.path().join("no-such-dir").join("out.jsonl");
        let error = JsonlWriter::open(Some(&missing_parent))
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot create --jsonl-out"), "{error}");
    }

    #[test]
    fn collect_grants_rejects_more_than_the_combined_cap() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("many.json");
        let items: Vec<serde_json::Value> = (0..16)
            .map(|index| {
                json!({
                    "id": format!("g{index}"),
                    "risk": "WorkspaceWrite",
                    "target": { "workspace_path_prefix": format!("f{index}.txt") },
                    "constraint": {},
                    "expires_at_ms": u64::MAX
                })
            })
            .collect();
        std::fs::write(&file, serde_json::Value::Array(items).to_string()).unwrap();
        let extra = hello_grant();
        let error = collect_grants(&[file], &[extra]).unwrap_err().to_string();
        assert!(error.contains("at most 16"), "{error}");
    }

    #[tokio::test]
    async fn headless_write_without_a_grant_refuses_and_does_not_imply_allow_all() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let composed = product_compose(root, &[], Arc::new(MockModelTransport), Some(4))
            .await
            .unwrap();
        let mut events = composed.subscribe();
        composed.instance.start().await.unwrap();
        let mut jsonl = Vec::new();
        let outcome = run_headless(
            composed.handle().clone(),
            &mut events,
            HeadlessAction::Prompt {
                text: "demo: write hello".into(),
                work: false,
            },
            Duration::from_secs(30),
            &mut jsonl,
        )
        .await
        .unwrap();
        composed.shutdown().await.unwrap();

        assert_eq!(outcome.exit, EXIT_APPROVAL_DENIED, "{outcome:?}");
        assert!(!root.join("hello.txt").exists());
        let text = String::from_utf8(jsonl).unwrap();
        assert!(text.contains("denied by approval policy"), "{text}");
        let end = session_end(&text);
        assert_eq!(end["exit"], 3);
        assert_eq!(end["status"], "approval_denied");
    }

    #[tokio::test]
    async fn headless_write_with_a_matching_grant_lands() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let composed = product_compose(
            root,
            &[hello_grant()],
            Arc::new(MockModelTransport),
            Some(4),
        )
        .await
        .unwrap();
        let mut events = composed.subscribe();
        composed.instance.start().await.unwrap();
        let mut jsonl = Vec::new();
        let outcome = run_headless(
            composed.handle().clone(),
            &mut events,
            HeadlessAction::Prompt {
                text: "demo: write hello".into(),
                work: false,
            },
            Duration::from_secs(30),
            &mut jsonl,
        )
        .await
        .unwrap();
        composed.shutdown().await.unwrap();

        assert_eq!(outcome.exit, EXIT_OK, "{outcome:?}");
        let written = std::fs::read_to_string(root.join("hello.txt")).unwrap();
        assert!(written.contains("hello from the demo agent"), "{written}");
        let end = session_end(&String::from_utf8(jsonl).unwrap());
        assert_eq!(end["exit"], 0);
        assert_eq!(end["approval_denied"], false);
        // MockModelTransport ends the turn without a durable completion:
        // the produced result is explicitly awaiting operator review.
        assert_eq!(end["task_state"], "awaiting_operator_review");
    }

    #[tokio::test]
    async fn headless_write_with_a_grant_file_lands() {
        let temp = tempfile::tempdir().unwrap();
        let grant_path = temp.path().join("grants.json");
        std::fs::write(&grant_path, hello_grant_value().to_string()).unwrap();
        let grants = load_grant_file(&grant_path).unwrap();
        let root = temp.path();
        let composed = product_compose(root, &grants, Arc::new(MockModelTransport), Some(4))
            .await
            .unwrap();
        let mut events = composed.subscribe();
        composed.instance.start().await.unwrap();
        let mut jsonl = Vec::new();
        let outcome = run_headless(
            composed.handle().clone(),
            &mut events,
            HeadlessAction::Prompt {
                text: "demo: write hello".into(),
                work: false,
            },
            Duration::from_secs(30),
            &mut jsonl,
        )
        .await
        .unwrap();
        composed.shutdown().await.unwrap();
        assert_eq!(outcome.exit, EXIT_OK, "{outcome:?}");
        let written = std::fs::read_to_string(root.join("hello.txt")).unwrap();
        assert!(written.contains("hello from the demo agent"), "{written}");
    }

    struct TwoRoundThenText;

    #[async_trait::async_trait]
    impl ModelTransport for TwoRoundThenText {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                streaming: true,
                tool_calls: true,
                max_output_tokens: 4096,
                context_window: None,
            }
        }

        async fn complete(&self, _request: ModelRequest) -> AgentResult<ModelOutput> {
            Ok(ModelOutput {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "round".into(),
                    name: "fs.list".into(),
                    arguments: json!({"path": "", "limit": 8}),
                }],
                usage: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn headless_round_budget_uses_exit_two() {
        let temp = tempfile::tempdir().unwrap();
        let composed = product_compose(temp.path(), &[], Arc::new(TwoRoundThenText), Some(1))
            .await
            .unwrap();
        let mut events = composed.subscribe();
        composed.instance.start().await.unwrap();
        let mut jsonl = Vec::new();
        let outcome = run_headless(
            composed.handle().clone(),
            &mut events,
            HeadlessAction::Prompt {
                text: "keep listing".into(),
                work: false,
            },
            Duration::from_secs(30),
            &mut jsonl,
        )
        .await
        .unwrap();
        composed.shutdown().await.unwrap();
        assert_eq!(outcome.exit, EXIT_ROUND_BUDGET, "{outcome:?}");
        let end = session_end(&String::from_utf8(jsonl).unwrap());
        assert_eq!(end["exit"], 2);
        assert_eq!(end["status"], "round_budget");
    }

    struct CountingList {
        step: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelTransport for CountingList {
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                streaming: true,
                tool_calls: true,
                max_output_tokens: 4096,
                context_window: None,
            }
        }

        async fn complete(&self, request: ModelRequest) -> AgentResult<ModelOutput> {
            let n = self.step.fetch_add(1, Ordering::SeqCst);
            let has_tool = request
                .messages
                .iter()
                .any(|message| message.role == agent_contracts::ModelRole::Tool);
            if n == 0 && !has_tool {
                return Ok(ModelOutput {
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "list-1".into(),
                        name: "fs.list".into(),
                        arguments: json!({"path": "", "limit": 8}),
                    }],
                    usage: Default::default(),
                });
            }
            Ok(ModelOutput {
                content: "[scripted] listed".into(),
                tool_calls: Vec::new(),
                usage: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn headless_work_composes_focus_and_one_user_message() {
        let temp = tempfile::tempdir().unwrap();
        let composed = product_compose(
            temp.path(),
            &[],
            Arc::new(CountingList {
                step: AtomicUsize::new(0),
            }),
            Some(4),
        )
        .await
        .unwrap();
        let mut events = composed.subscribe();
        composed.instance.start().await.unwrap();
        let mut jsonl = Vec::new();
        let outcome = run_headless(
            composed.handle().clone(),
            &mut events,
            HeadlessAction::Prompt {
                text: "list the workspace".into(),
                work: true,
            },
            Duration::from_secs(30),
            &mut jsonl,
        )
        .await
        .unwrap();
        composed.shutdown().await.unwrap();
        assert_eq!(outcome.exit, EXIT_OK, "{outcome:?}");
        let text = String::from_utf8(jsonl).unwrap();
        assert!(text.contains("focus_changed"), "{text}");
        assert!(text.contains("user_message_accepted"), "{text}");
        assert!(
            text.contains("task.manage") || text.contains("task_tool_requirements_changed"),
            "{text}"
        );
    }
}
