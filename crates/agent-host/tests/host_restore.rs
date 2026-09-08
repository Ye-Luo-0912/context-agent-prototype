//! N5 end-to-end: the host's `--restore-latest` startup consumes the real
//! runtime checkpoint store, not bare JSON.
//!
//! The save side composes a real runtime, runs one turn, and saves through
//! the same atomic envelope store the TUI's `/checkpoint` uses, so the
//! artifact on disk is the genuine `runtime-checkpoint-envelope-v1` form
//! (header + checksum + payload). The restore side then runs exactly what
//! `agent-host --restore-latest` runs —
//! [`agent_host::resolve_latest_verified_checkpoint`] followed by the full
//! [`agent_runtime::RuntimeInstance::restore`] transaction — and the
//! original task identity must come back with an explicit continue
//! available. A corrupt store must fail closed instead of restoring a
//! guessed state.

use std::sync::Arc;
use std::time::Duration;

use agent_compose::{ComposeConfig, ContextPolicy, HostToolPolicyRegistry, compose};
use agent_contracts::{ModelTransport, RunId, RuntimeEvent, TaskId};
use agent_core::{PolicyApprovalGate, TaskApprovalGate};
use agent_runtime::{CheckpointStore, RUNTIME_CHECKPOINT_VERSION, RunMetadata, RuntimeCheckpoint};
use agent_workspace::Workspace;

/// The demo mock the host binary itself uses in demo mode: one canned
/// read-only `fs.list` round, then a plain final, so the turn is real
/// (kernel + tools + approval) without needing provider credentials or
/// standing write grants.
fn demo_model() -> Arc<dyn ModelTransport> {
    Arc::new(agent_compose::MockModelTransport)
}

/// The host binary's composition shape, reduced to what a restore test
/// needs: persistent journal and effect-reservation journal across the
/// restart, real builtin tools, read-only base approval (the writes would
/// need grants; this flow stays read-only), and the workspace artifact
/// store that backs the automatic safe-point store.
async fn host_config(
    root: &std::path::Path,
    model: Arc<dyn ModelTransport>,
) -> anyhow::Result<ComposeConfig> {
    let workspace = Workspace::open(root).await?;
    let journal = Arc::new(
        agent_storage::FileEventJournal::open(workspace.state_dir().join("traces")).await?,
    );
    let recipes = Arc::new(tool_runtime::VerificationRecipes::discover(&workspace)?);
    let host_policies = Arc::new(
        HostToolPolicyRegistry::with_builtins_and_verification(&recipes)
            .map_err(anyhow::Error::msg)?,
    );
    let task_gate = Arc::new(
        TaskApprovalGate::new(Arc::new(PolicyApprovalGate::read_only()))
            .with_host_policies(host_policies.clone()),
    );
    // Dynamic (SimpleContextEngine) is the restore-capable product policy:
    // it tracks the focused task, which the kernel's restore-side focus
    // authority check requires. The rolling baseline engine does not track
    // focus, so an active-task checkpoint is (correctly, but irrecoverably)
    // refused under `--context-policy rolling` — a known pre-existing gap
    // outside this ticket's files.
    let context_engine =
        agent_compose::build_context_engine(ContextPolicy::Dynamic, workspace.state_dir(), None)
            .await?;
    let base_tools = Arc::new(
        tool_runtime::BuiltinToolDispatcher::with_config_and_verification_recipes(
            workspace.clone(),
            Default::default(),
            (*recipes).clone(),
        ),
    );
    let reservation = workspace
        .state_dir()
        .join("authority")
        .join("broker-reservations.jsonl");
    Ok(ComposeConfig {
        provider_profile_digest: None,
        defer_proof_refresh: false,
        shadow_context_frame: false,
        workspace: workspace.clone(),
        context_engine,
        model,
        approval: task_gate,
        base_tools,
        capability_aware: true,
        journal: Some(journal),
        artifact_store: Some(Arc::new(workspace.clone())),
        output_broker: None,
        max_tool_rounds: None,
        project_task_progress: true,
        project_settlement: false,
        settlement_projection_diagnostics: false,
        project_completion_opportunity: false,
        recovery_surface: false,
        host_policies: Some(host_policies),
        effect_reservation_journal: Some(reservation),
        verification_recipes: Some(recipes.clone()),
        project_proof_refresh: false,
        // Harness composition: no watchdog dispatch in this executable.
        host_death_watchdog: false,
        mcp_servers: Vec::new(),
        plugins: None,
    })
}

async fn wait_for(
    events: &mut tokio::sync::broadcast::Receiver<agent_contracts::RuntimeEventEnvelope>,
    matches: impl Fn(&RuntimeEvent) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if events
            .try_recv()
            .is_ok_and(|envelope| matches(&envelope.event))
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runtime never {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Save → end process → `--restore-latest` → explicit continue, across two
/// real compositions on one workspace. The saved artifact is the store's
/// genuine envelope form, and whichever newest artifact verifies is the one
/// restored; the original task identity must survive both.
#[tokio::test]
async fn restore_latest_resumes_the_saved_task_and_continue_works() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();

    // ---- Session 1: one real turn, then a store envelope checkpoint. ----
    let composed = compose(host_config(&root, demo_model()).await?).await?;
    let checkpoints_dir = composed.workspace.state_dir().join("checkpoints");
    let mut events = composed.subscribe();
    let handle = composed.handle().clone();
    handle.start().await?;
    handle.set_focus("demo: list files".into()).await?;
    handle.user_message("demo: list files".into()).await?;
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::TurnCompleted),
        "complete the first turn",
    )
    .await;

    let saved = composed.instance.checkpoint().await?;
    let expected_task: TaskId = saved
        .current_task_id
        .expect("an active task after the first turn");
    // The deliberate save rides the runtime's own atomic envelope store —
    // the same write the TUI's `/checkpoint` performs.
    let store = CheckpointStore::new(&checkpoints_dir);
    let stored = store
        .write_atomic(&serde_json::to_vec(&saved)?)
        .await
        .map_err(anyhow::Error::from)?;
    assert!(
        stored.artifact.starts_with("checkpoint-"),
        "store artifact naming: {}",
        stored.artifact
    );
    let on_disk = std::fs::read(checkpoints_dir.join(&stored.artifact))?;
    let header = std::str::from_utf8(&on_disk)?
        .lines()
        .next()
        .unwrap()
        .to_string();
    assert!(
        header.contains("runtime-checkpoint-envelope-v1"),
        "the saved artifact must be a real envelope, got header: {header}"
    );
    composed.shutdown().await?;

    // ---- Session 2: --restore-latest's exact code path, then continue. ----
    let composed = compose(host_config(&root, demo_model()).await?).await?;
    let mut events = composed.subscribe();
    composed.instance.start().await?;
    let (checkpoint, resolved) =
        agent_host::resolve_latest_verified_checkpoint(&checkpoints_dir).await?;
    assert_eq!(
        resolved.parent(),
        Some(checkpoints_dir.as_path()),
        "the resolved checkpoint must come from the workspace store"
    );
    assert_eq!(
        checkpoint.current_task_id.as_ref().map(|id| id.to_string()),
        Some(expected_task.to_string()),
        "the newest verifiable checkpoint must carry the saved task identity"
    );
    // The full cross-plane restore transaction — the same call the host
    // binary and the TUI make; no field picking.
    composed
        .instance
        .restore(checkpoint)
        .await
        .map_err(anyhow::Error::from)?;
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::RuntimeRestored { .. }),
        "commit the restore",
    )
    .await;

    // The original task identity is back in the restored task table.
    let tasks = composed.handle().list_tasks().await?;
    let restored = tasks
        .iter()
        .find(|task| task.id.to_string() == expected_task.to_string())
        .expect("the restored task table must contain the original task");
    assert_eq!(restored.goal, "demo: list files");

    // Explicit continue is usable: a fresh turn runs and completes on the
    // restored task.
    let continued = composed
        .handle()
        .continue_active_task()
        .await
        .map_err(anyhow::Error::from)?;
    assert_eq!(continued.to_string(), expected_task.to_string());
    wait_for(
        &mut events,
        |event| matches!(event, RuntimeEvent::TurnCompleted),
        "complete the continued turn",
    )
    .await;
    composed.shutdown().await?;
    Ok(())
}

/// A decodable idle checkpoint payload, as any real runtime capture would
/// serialize it (no active task, no authority marker). Only its transport
/// matters here: selection and verification semantics, not provenance.
fn idle_checkpoint_payload() -> Vec<u8> {
    let checkpoint = RuntimeCheckpoint {
        version: RUNTIME_CHECKPOINT_VERSION,
        run_metadata: RunMetadata {
            run_id: RunId::new(),
            created_at_ms: 1,
            provider_profile_digest: String::new(),
        },
        tasks: agent_runtime::TaskManagerSnapshot {
            tasks: Vec::new(),
            active: None,
            completed: Vec::new(),
        },
        current_task_id: None,
        focus_revision: 0,
        last_surface_revision: 0,
        context: serde_json::json!({}),
        capabilities: Vec::new(),
        authority: None,
        snapshot_sequence: 0,
        capability_generation: 0,
        unresolved_ack_debts: Vec::new(),
        event_cover_seq: 0,
        terminal_commit: false,
    };
    serde_json::to_vec(&checkpoint).unwrap()
}

/// Newest artifact corrupt → skipped with a note, the last verifiable one
/// is restored; once every candidate fails, selection fails closed instead
/// of degrading silently.
#[tokio::test]
async fn restore_latest_skips_a_corrupt_newest_and_picks_the_last_verifiable() -> anyhow::Result<()>
{
    let temp = tempfile::tempdir()?;
    let checkpoints_dir = temp.path().to_path_buf();
    let store = CheckpointStore::new(&checkpoints_dir);
    let payload = idle_checkpoint_payload();

    let older = store
        .write_atomic(&payload)
        .await
        .map_err(anyhow::Error::from)?;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let newest = store
        .write_atomic(&payload)
        .await
        .map_err(anyhow::Error::from)?;
    assert_ne!(older.artifact, newest.artifact);

    // Corrupt the newest artifact's payload bytes: the envelope header
    // stays, the checksum no longer matches.
    let newest_path = checkpoints_dir.join(&newest.artifact);
    let mut bytes = std::fs::read(&newest_path)?;
    let last = bytes.len() - 1;
    bytes[last] ^= 0x5A;
    std::fs::write(&newest_path, &bytes)?;

    let (checkpoint, resolved) =
        agent_host::resolve_latest_verified_checkpoint(&checkpoints_dir).await?;
    assert_eq!(
        resolved.file_name().unwrap().to_string_lossy(),
        older.artifact,
        "the corrupt newest candidate must be skipped for the last verifiable one"
    );
    assert_eq!(checkpoint.version, RUNTIME_CHECKPOINT_VERSION);

    // All candidates broken → fail closed.
    let older_path = checkpoints_dir.join(&older.artifact);
    let mut bytes = std::fs::read(&older_path)?;
    bytes.truncate(bytes.len() - 8);
    std::fs::write(&older_path, &bytes)?;
    let error = agent_host::resolve_latest_verified_checkpoint(&checkpoints_dir)
        .await
        .expect_err("a store with no verifiable candidate must fail closed");
    assert!(
        error.to_string().contains("failed verification"),
        "unexpected error: {error}"
    );
    Ok(())
}

/// An empty store is a configuration error, and a store whose only
/// artifacts decode to nothing (checksum-valid envelope, non-checkpoint
/// payload) refuses startup rather than restoring a guess.
#[tokio::test]
async fn restore_latest_fails_closed_on_an_empty_or_unreadable_store() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let checkpoints_dir = temp.path().to_path_buf();

    let error = agent_host::resolve_latest_verified_checkpoint(&checkpoints_dir)
        .await
        .expect_err("an empty store must be a configuration error");
    assert!(
        error.to_string().contains("no verifiable checkpoints"),
        "unexpected error: {error}"
    );

    let store = CheckpointStore::new(&checkpoints_dir);
    store
        .write_atomic(b"definitely not a runtime checkpoint payload")
        .await
        .map_err(anyhow::Error::from)?;
    let error = agent_host::resolve_latest_verified_checkpoint(&checkpoints_dir)
        .await
        .expect_err("a store with only undecodable artifacts must fail closed");
    assert!(
        error.to_string().contains("failed verification"),
        "unexpected error: {error}"
    );
    Ok(())
}
