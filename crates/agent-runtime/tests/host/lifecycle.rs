use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use agent_contracts::{
    AgentError, CancellationToken, CapabilityTransport, RunId, ToolCall, ToolDispatcher,
    ToolExecutionRequest,
};
use agent_runtime::{CapabilityAwareDispatcher, CapabilityRunState, ModuleHost, ToolModule};
use serde_json::json;

use crate::harness::*;

#[tokio::test]
async fn disabled_capabilities_cannot_load_or_run_until_enabled() {
    let mut host = ModuleHost::new();
    let registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        registry.clone(),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // An external capability is registered Disabled: nothing may load or
    // run it.
    host.register_capability(Arc::new(DemoCapability::with_tool_names(
        "ext-gated",
        &["ext-gated.run"],
        CapabilityTransport::Process {
            program: "plugin".into(),
        },
    )))
    .unwrap();

    let tools = host.registry().tool_provider().unwrap();
    let error = dispatcher
        .load_tool("ext-gated.run")
        .expect_err("loading a disabled capability must fail");
    assert!(error.to_string().contains("disabled"), "{error}");

    // Enabling makes it loadable and runnable.
    registry.enable("ext-gated").await.unwrap();
    dispatcher.load_tool("ext-gated.run").unwrap();
    let output = execute(tools, "ext-gated.run").await;
    assert!(output.ok);
    assert_eq!(output.model_content, "demo handled ext-gated.run");

    host.stop().await.unwrap();
}

#[tokio::test]
async fn activation_can_be_disabled_and_quarantined_after_use() {
    let mut host = ModuleHost::new();
    let registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        registry.clone(),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // Trusted builtin capability starts Enabled and usable.
    host.register_capability(Arc::new(DemoCapability::with_tool_names(
        "flaky",
        &["flaky.run"],
        CapabilityTransport::Builtin,
    )))
    .unwrap();
    let tools = host.registry().tool_provider().unwrap();
    dispatcher.load_tool("flaky.run").unwrap();
    assert!(execute(tools.clone(), "flaky.run").await.ok);

    // After misbehavior the operator disables it: tools leave the surface
    // and calls are blocked at the gate.
    registry.disable("flaky").await.unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(
        !names.contains(&"flaky.run".to_string()),
        "a disabled capability must leave the model surface"
    );
    let error = execute_raw(tools.clone(), "flaky.run").await;
    assert!(
        error.contains("disabled"),
        "invoking a disabled capability must fail at the gate: {error}"
    );

    // Quarantine is the same gate, with its own label.
    registry.enable("flaky").await.unwrap();
    registry.quarantine("flaky").await.unwrap();
    let error = execute_raw(tools, "flaky.run").await;
    assert!(error.contains("quarantined"), "{error}");

    host.stop().await.unwrap();
}

#[tokio::test]
async fn concurrent_ensure_started_serializes_to_a_single_start() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    let starts = Arc::new(AtomicUsize::new(0));
    host.register_capability(Arc::new(InstrumentedCapability {
        manifest: instrumented_manifest("slow"),
        starts: starts.clone(),
        fail_first: Arc::new(AtomicBool::new(false)),
    }))
    .unwrap();

    // Both callers race the same transition; the per-capability lifecycle
    // lock must collapse them into exactly one `start()`.
    let (a, b) = tokio::join!(
        registry.ensure_started("slow"),
        registry.ensure_started("slow"),
    );
    a.unwrap();
    b.unwrap();

    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "concurrent ensure_started calls must produce exactly one start()"
    );
    let entry = registry
        .catalog()
        .into_iter()
        .find(|entry| entry.id == "slow")
        .expect("catalog must list the capability");
    assert_eq!(
        entry.run_state,
        CapabilityRunState::Started,
        "a successful start must leave the capability Started"
    );
}

#[tokio::test]
async fn failed_start_is_observable_and_a_later_start_retries() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    let starts = Arc::new(AtomicUsize::new(0));
    host.register_capability(Arc::new(InstrumentedCapability {
        manifest: instrumented_manifest("flaky"),
        starts: starts.clone(),
        fail_first: Arc::new(AtomicBool::new(true)),
    }))
    .unwrap();

    let first = registry.ensure_started("flaky").await;
    assert!(first.is_err(), "the instrumented first start must fail");
    assert_eq!(
        registry
            .catalog()
            .into_iter()
            .find(|entry| entry.id == "flaky")
            .map(|entry| entry.run_state),
        Some(CapabilityRunState::Failed),
        "a failed start must be observable as Failed"
    );

    // The failure is not sticky: a later start retries the transition.
    registry.ensure_started("flaky").await.unwrap();
    assert_eq!(starts.load(Ordering::SeqCst), 2);
    assert_eq!(
        registry
            .catalog()
            .into_iter()
            .find(|entry| entry.id == "flaky")
            .map(|entry| entry.run_state),
        Some(CapabilityRunState::Started)
    );
}

/// The permission Core grants nothing undeclared: the runtime builds the
/// invocation context from the manifest's declared permissions alone, so a
/// capability that never declared a workspace permission receives no
/// workspace handle at all, and one that declared only reads cannot write —
/// blocked by construction, not by trust.
#[tokio::test]
async fn undeclared_permissions_receive_no_handle() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let mut host = ModuleHost::new();
    let registry = host.capability_registry();

    // Four capabilities with different declared grants, plus one declaring
    // an unknown permission string — which the registry now refuses up
    // front: unknown access is denied by refusing the declaration.
    let no_ws = Arc::new(ContextCapturingCapability::with_permissions(
        "no-ws",
        &[agent_contracts::RUNTIME_CONTEXT_CONTROL],
    ));
    let no_ws_captured = no_ws.captured.clone();
    let read_only = Arc::new(ContextCapturingCapability::with_permissions(
        "read-only",
        &["workspace:read"],
    ));
    let read_only_captured = read_only.captured.clone();
    let write_ws = Arc::new(ContextCapturingCapability::with_permissions(
        "write-ws",
        &["workspace:write"],
    ));
    let write_ws_captured = write_ws.captured.clone();
    let unknown =
        ContextCapturingCapability::with_permissions("unknown-perm", &["totally-made-up:perm"]);
    let registration = host.register_capability(Arc::new(unknown));
    assert!(
        registration.is_err(),
        "an unknown permission string must be refused at registration"
    );
    assert!(
        registration
            .unwrap_err()
            .to_string()
            .contains("unknown permission"),
        "the refusal must name the unknown permission"
    );
    for capability in [no_ws, read_only, write_ws] {
        host.register_capability(capability).unwrap();
    }

    let dispatcher = Arc::new(CapabilityAwareDispatcher::with_workspace(
        Arc::new(StubTools),
        registry.clone(),
        Some(workspace.clone()),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // Builtin capabilities are Enabled on registration, so the full model
    // path works: load each tool, call it, inspect what it received.
    let tools = host.registry().tool_provider().unwrap();
    for id in ["no-ws", "read-only", "write-ws"] {
        registry.load_tool(&format!("{id}.run")).unwrap();
        let output = execute(tools.clone(), &format!("{id}.run")).await;
        assert!(output.ok, "{id}: the recording call must succeed");
    }

    // 1. No workspace permission declared -> no workspace handle at all.
    let ctx = no_ws_captured.lock().unwrap().take().unwrap();
    assert_eq!(
        ctx.granted_permissions,
        [agent_contracts::RUNTIME_CONTEXT_CONTROL]
    );
    assert!(
        ctx.workspace.is_none(),
        "a capability that declared no workspace permission must receive no workspace handle"
    );
    assert!(ctx.artifacts.is_none(), "no artifact permission declared");

    // 2. Write declared -> a staged-only handle: the direct write path is
    //    refused (a mutation applied during invoke would bypass the
    //    generation fence, cancellation and effect rollback), and the
    //    mutation must be prepared as an Effect and committed by the core.
    //    This lands the file the read-only capability will read back below.
    let ctx = write_ws_captured.lock().unwrap().take().unwrap();
    assert_eq!(ctx.granted_permissions, ["workspace:write"]);
    let handle = ctx
        .workspace
        .expect("workspace:write must receive a handle");
    let error = handle
        .write("granted.txt", b"x")
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("must be staged"),
        "the direct write must be refused and name the staged path: {error}"
    );
    let effect = handle
        .prepare_write("granted.txt", b"granted content")
        .await
        .expect("prepare_write must stage the mutation");
    let receipt = effect.commit().await;
    assert!(
        matches!(
            &receipt,
            agent_contracts::EffectReceipt::Applied {
                durability: agent_contracts::EffectDurability::Durable,
                ..
            }
        ),
        "the staged effect commits durably: {receipt:?}"
    );
    assert_eq!(
        handle.read("granted.txt").await.unwrap(),
        b"granted content"
    );
    let bounded = handle.read_bounded("granted.txt", 7).await.unwrap();
    assert_eq!(bounded.content, b"granted");
    assert_eq!(bounded.byte_len, b"granted content".len() as u64);
    assert!(bounded.truncated);

    // 3. Read-only declared -> a read-only handle: reads work, both write
    //    paths are blocked with an error naming the missing grant.
    let ctx = read_only_captured.lock().unwrap().take().unwrap();
    assert_eq!(ctx.granted_permissions, ["workspace:read"]);
    let handle = ctx.workspace.expect("workspace:read must receive a handle");
    assert_eq!(
        handle.read("granted.txt").await.unwrap(),
        b"granted content",
        "the read-only handle must still read the workspace"
    );
    assert_eq!(
        handle
            .read_bounded("granted.txt", 1024)
            .await
            .unwrap()
            .content,
        b"granted content",
        "the read-only wrapper must preserve bounded-read access"
    );
    let error = handle.write("x.txt", b"x").await.unwrap_err().to_string();
    assert!(
        error.contains("workspace:write was not granted"),
        "the write refusal must name the grant: {error}"
    );
    let error = match handle.prepare_write("x.txt", b"x").await {
        Err(error) => error.to_string(),
        Ok(_) => panic!("prepare_write must be refused without the grant"),
    };
    assert!(
        error.contains("workspace:write was not granted"),
        "the staged-write refusal must name the grant: {error}"
    );

    // 4. An unknown permission string is refused at registration (asserted
    //    above) — unknown access is denied by refusing the declaration,
    //    before any handle could ever be granted.

    host.stop().await.unwrap();
}

#[tokio::test]
async fn disable_waits_for_a_start_that_won_the_lifecycle_lock_and_then_stops() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let starts = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    host.register_capability(Arc::new(LifecycleProbeCapability {
        manifest: probe_manifest("probe", &[]),
        start_entered: Some(entered_tx),
        start_gate: Some(gate.clone()),
        starts: starts.clone(),
        stops: stops.clone(),
        live: Arc::new(AtomicBool::new(true)),
        invoke_hangs: Arc::new(AtomicBool::new(false)),
        invoke_entered: None,
        invoke_result: Arc::new(Mutex::new(Ok(()))),
    }))
    .unwrap();

    // The start wins the lifecycle lock and is held open at the gate.
    let start_task = tokio::spawn({
        let registry = registry.clone();
        async move { registry.ensure_started("probe").await }
    });
    entered_rx.recv().await.unwrap(); // start() is inside, holding the lock

    // The disable queues behind it; when the gate opens, it settles the
    // just-started service exactly once.
    let disable_task = tokio::spawn({
        let registry = registry.clone();
        async move { registry.disable("probe").await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    gate.wait().await;

    start_task.await.unwrap().unwrap();
    disable_task.await.unwrap().unwrap();

    assert_eq!(starts.load(Ordering::SeqCst), 1, "exactly one start()");
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "the disable must stop the started service once"
    );
    let entry = registry
        .catalog()
        .into_iter()
        .find(|entry| entry.id == "probe")
        .expect("catalog must list the capability");
    assert_eq!(
        entry.run_state,
        CapabilityRunState::Stopped,
        "a settled disable must leave the record Stopped"
    );
    assert!(
        registry.ensure_started("probe").await.is_err(),
        "a disabled capability must refuse a later start"
    );
}

#[tokio::test]
async fn quarantine_cancels_an_in_flight_invocation_and_stops_the_service() {
    let mut host = ModuleHost::new();
    let registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        registry.clone(),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let stops = Arc::new(AtomicUsize::new(0));
    host.register_capability(Arc::new(LifecycleProbeCapability {
        manifest: probe_manifest("probe", &["probe.run"]),
        start_entered: None,
        start_gate: None,
        starts: Arc::new(AtomicUsize::new(0)),
        stops: stops.clone(),
        live: Arc::new(AtomicBool::new(true)),
        invoke_hangs: Arc::new(AtomicBool::new(true)),
        invoke_entered: Some(entered_tx),
        invoke_result: Arc::new(Mutex::new(Ok(()))),
    }))
    .unwrap();
    host.start().await.unwrap();
    let _tools = host.registry().tool_provider().unwrap();
    dispatcher.load_tool("probe.run").unwrap();

    // The invocation blocks inside the capability on its cancellation
    // handle — the in-flight state the quarantine must settle.
    let executing = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move {
            dispatcher
                .execute(ToolExecutionRequest {
                    run_id: RunId::new(),
                    call: ToolCall {
                        id: "call-probe".into(),
                        name: "probe.run".into(),
                        arguments: json!({}),
                    },
                    effect_context: None,
                    cancel: CancellationToken::new(),
                })
                .await
        }
    });
    entered_rx.recv().await.unwrap();

    registry.quarantine("probe").await.unwrap();

    let result = executing.await.unwrap();
    assert!(
        matches!(result, Err(AgentError::Cancelled)),
        "quarantine must settle the in-flight invocation: {result:?}"
    );
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "quarantine must stop the service exactly once"
    );
    let entry = registry
        .catalog()
        .into_iter()
        .find(|entry| entry.id == "probe")
        .expect("catalog must list the capability");
    assert_eq!(entry.run_state, CapabilityRunState::Stopped);

    host.stop().await.unwrap();
}

#[tokio::test]
async fn a_poisoned_invoke_downgrades_the_record_and_the_next_start_restarts() {
    let mut host = ModuleHost::new();
    let registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        registry.clone(),
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    let starts = Arc::new(AtomicUsize::new(0));
    let dead = Arc::new(AtomicBool::new(false));
    host.register_capability(Arc::new(LifecycleProbeCapability {
        manifest: probe_manifest("probe", &["probe.run"]),
        start_entered: None,
        start_gate: None,
        starts: starts.clone(),
        stops: Arc::new(AtomicUsize::new(0)),
        live: dead.clone(),
        invoke_hangs: Arc::new(AtomicBool::new(false)),
        invoke_entered: None,
        invoke_result: Arc::new(Mutex::new(Err("poisoned connection".into()))),
    }))
    .unwrap();
    host.start().await.unwrap();
    let _tools = host.registry().tool_provider().unwrap();
    dispatcher.load_tool("probe.run").unwrap();

    // The connection dies while the call fails, so the invoke sees a dead
    // adapter and the Started record must downgrade.
    dead.store(false, Ordering::SeqCst);
    let error = dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "call-probe".into(),
                name: "probe.run".into(),
                arguments: json!({}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .expect_err("the dead invocation must fail");
    assert!(error.to_string().contains("poisoned connection"), "{error}");
    assert_eq!(
        registry
            .catalog()
            .into_iter()
            .find(|entry| entry.id == "probe")
            .map(|entry| entry.run_state),
        Some(CapabilityRunState::Failed),
        "a failed invoke behind a dead connection must downgrade Started to Failed"
    );

    // The next start goes through the start path again (the adapter's
    // restart path), now that the connection is alive.
    dead.store(true, Ordering::SeqCst);
    registry.ensure_started("probe").await.unwrap();
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "the restart must call start() again"
    );
    assert_eq!(
        registry
            .catalog()
            .into_iter()
            .find(|entry| entry.id == "probe")
            .map(|entry| entry.run_state),
        Some(CapabilityRunState::Started)
    );

    // A domain failure with a live connection does not downgrade: only a
    // dead connection consumes the restart path.
    let healthy_starts = Arc::new(AtomicUsize::new(0));
    host.register_capability(Arc::new(LifecycleProbeCapability {
        manifest: probe_manifest("healthy", &["healthy.run"]),
        start_entered: None,
        start_gate: None,
        starts: healthy_starts.clone(),
        stops: Arc::new(AtomicUsize::new(0)),
        live: Arc::new(AtomicBool::new(true)),
        invoke_hangs: Arc::new(AtomicBool::new(false)),
        invoke_entered: None,
        invoke_result: Arc::new(Mutex::new(Err("domain refusal".into()))),
    }))
    .unwrap();
    dispatcher.load_tool("healthy.run").unwrap();
    let error = dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "call-healthy".into(),
                name: "healthy.run".into(),
                arguments: json!({}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .expect_err("the refused call must fail");
    assert!(error.to_string().contains("domain refusal"), "{error}");
    assert_eq!(
        registry
            .catalog()
            .into_iter()
            .find(|entry| entry.id == "healthy")
            .map(|entry| entry.run_state),
        Some(CapabilityRunState::Started),
        "a live connection must keep the Started record"
    );
    registry.ensure_started("healthy").await.unwrap();
    assert_eq!(
        healthy_starts.load(Ordering::SeqCst),
        1,
        "a live Started record must not re-start"
    );

    host.stop().await.unwrap();
}
