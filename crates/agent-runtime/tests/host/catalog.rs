use std::sync::{Arc, Mutex};

use agent_contracts::{
    CancellationToken, CapabilityLifecycle, RunId, ToolCall, ToolDispatcher, ToolExecutionRequest,
    ToolLifecycle, ToolOutcome,
};
use agent_runtime::{CapabilityAwareDispatcher, ModuleHost};
use serde_json::json;

use crate::harness::*;

#[tokio::test]
async fn snapshot_generation_tracks_dynamic_capability_changes() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    let dispatcher = CapabilityAwareDispatcher::new(Arc::new(StubTools), registry.clone());

    let before = dispatcher.snapshot().generation;
    let capability = DemoCapability::new(
        "gen-a",
        CapabilityLifecycle::Lazy,
        Arc::new(Mutex::new(false)),
    );
    registry.register(Arc::new(capability)).unwrap();
    let after_register = dispatcher.snapshot().generation;
    assert!(
        after_register > before,
        "registration must bump the surface generation"
    );

    registry.load_tool("gen-a.demo").unwrap();
    let after_load = dispatcher.snapshot().generation;
    assert!(
        after_load > after_register,
        "loading must bump the generation"
    );

    registry.unload_tool("gen-a.demo").unwrap();
    let after_unload = dispatcher.snapshot().generation;
    assert!(
        after_unload > after_load,
        "unloading must bump the generation"
    );

    registry.enable("gen-a").unwrap();
    let after_activate = dispatcher.snapshot().generation;
    assert!(
        after_activate > after_unload,
        "activation changes must bump the generation"
    );
}

#[test]
fn snapshot_never_silently_trims_a_fail_closed_required_schema() {
    let dispatcher = CapabilityAwareDispatcher::new(
        Arc::new(RequiredLargeTools),
        Arc::new(agent_runtime::CapabilityRegistry::new()),
    );

    let snapshot = dispatcher.snapshot();
    assert!(
        snapshot
            .specs
            .iter()
            .any(|spec| spec.name == "required.large"),
        "the initial schema cap must preserve fail-closed schemas"
    );
    assert!(
        agent_runtime::approx_layer_tokens(&snapshot.specs)
            > agent_runtime::budget::MAX_TOOL_SURFACE_TOKENS,
        "an oversized mandatory set remains visible so the actor can fail explicitly"
    );
}

#[tokio::test]
async fn unified_search_pages_and_spills_to_the_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Arc::new(agent_workspace::Workspace::open(dir.path()).await.unwrap());
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    for id in ["ext.a", "ext.b", "ext.c"] {
        registry
            .register(Arc::new(DemoCapability::new(
                id,
                CapabilityLifecycle::Lazy,
                Arc::new(Mutex::new(false)),
            )))
            .unwrap();
    }
    let dispatcher = CapabilityAwareDispatcher::with_workspace(
        Arc::new(StubTools),
        registry.clone(),
        Some(workspace.clone()),
    );

    let output = dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "c".into(),
                name: agent_contracts::CAPABILITY_MANAGE.into(),
                arguments: json!({"op": "search", "limit": 2}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    let output = match output {
        ToolOutcome::Value(output) => output,
        other => panic!("capability.manage search must return a plain value, got {other:?}"),
    };
    assert!(output.ok);
    assert!(
        output.artifact_ref.is_some(),
        "a catalog larger than the page must spill to an artifact"
    );
    assert!(
        output.model_content.lines().count() <= 2,
        "the model must only see the bounded page: {}",
        output.model_content
    );
    assert_eq!(output.metadata["has_more"], true);
    assert_eq!(
        output.metadata["total"], 4,
        "fs.read + three capability tools"
    );
}

#[tokio::test]
async fn unified_search_matches_description_and_reports_not_found() {
    let dispatcher = CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        Arc::new(agent_runtime::CapabilityRegistry::new()),
    );
    let before: Vec<_> = dispatcher
        .snapshot()
        .specs
        .iter()
        .map(|s| s.name.clone())
        .collect();
    let search = dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "c".into(),
                name: agent_contracts::CAPABILITY_MANAGE.into(),
                arguments: json!({"op": "search", "query": "STUB BUILTIN"}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    let search = match search {
        ToolOutcome::Value(output) => output,
        other => panic!("expected value, got {other:?}"),
    };
    assert!(search.ok);
    assert!(
        search.model_content.contains("fs.read"),
        "capability search must match description case-insensitively: {}",
        search.model_content
    );
    let after: Vec<_> = dispatcher
        .snapshot()
        .specs
        .iter()
        .map(|s| s.name.clone())
        .collect();
    assert_eq!(
        before, after,
        "search must not load a tool onto the surface"
    );

    let inspect = dispatcher
        .execute(ToolExecutionRequest {
            run_id: RunId::new(),
            call: ToolCall {
                id: "c2".into(),
                name: agent_contracts::CAPABILITY_MANAGE.into(),
                arguments: json!({"op": "inspect", "name": "missing.tool"}),
            },
            effect_context: None,
            cancel: CancellationToken::new(),
        })
        .await
        .unwrap();
    let inspect = match inspect {
        ToolOutcome::Value(output) => output,
        other => panic!("expected value, got {other:?}"),
    };
    assert!(!inspect.ok);
    assert_eq!(inspect.metadata["miss"], "not_found");
}

#[tokio::test]
async fn catalog_rows_are_cached_and_invalidate_on_surface_changes() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    registry
        .register(Arc::new(DemoCapability::new(
            "cache-demo",
            CapabilityLifecycle::Lazy,
            Arc::new(Mutex::new(false)),
        )))
        .unwrap();

    // An unchanged catalog serves the cached rows: repeated discovery
    // reads must not rebuild the derived metadata per call.
    let first = registry.catalog_rows();
    let second = registry.catalog_rows();
    assert!(
        Arc::ptr_eq(&first, &second),
        "unchanged catalog must serve the cached rows"
    );

    // A surface change (load) invalidates the cache and the fresh rows
    // reflect the new lifecycle state.
    registry.load_tool("cache-demo.demo").unwrap();
    let third = registry.catalog_rows();
    assert!(
        !Arc::ptr_eq(&first, &third),
        "a load must invalidate the cache"
    );
    assert!(
        third
            .iter()
            .any(|row| row.name == "cache-demo.demo" && row.state == ToolLifecycle::Loaded),
        "a loaded tool must report Loaded in the fresh rows"
    );

    // An executing tool flips its row to Active; the cache must not serve
    // a stale Loaded state across a call boundary.
    registry.mark_active("cache-demo.demo");
    let fourth = registry.catalog_rows();
    assert!(
        !Arc::ptr_eq(&third, &fourth),
        "an active mark must invalidate the cache"
    );
    assert!(
        fourth
            .iter()
            .any(|row| row.name == "cache-demo.demo" && row.state == ToolLifecycle::Active),
        "an executing tool must report Active in the fresh rows"
    );
    registry.mark_idle("cache-demo.demo");
    let fifth = registry.catalog_rows();
    assert!(
        !Arc::ptr_eq(&fourth, &fifth),
        "an idle mark must invalidate the cache"
    );
    assert!(
        fifth
            .iter()
            .any(|row| row.name == "cache-demo.demo" && row.state == ToolLifecycle::Loaded),
        "an idle tool returns to Loaded in the fresh rows"
    );
}
