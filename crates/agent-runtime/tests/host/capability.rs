use std::sync::{Arc, Mutex};

use agent_contracts::{
    CapabilityActivation, CapabilityLifecycle, CapabilityStatus, CapabilityTransport,
    ToolDispatcher, ToolLifecycle, ToolRisk, ToolSpec,
};
use agent_runtime::{APPROVAL_POLICY, CapabilityAwareDispatcher, ModuleHost, ToolModule};
use serde_json::json;

use crate::harness::*;

#[tokio::test]
async fn dynamic_capabilities_reach_the_model_and_route_calls() {
    let mut host = ModuleHost::new();
    let capability_registry = host.capability_registry();
    let started = Arc::new(Mutex::new(false));
    host.register_capability(Arc::new(DemoCapability::new(
        "demo",
        CapabilityLifecycle::Eager,
        started.clone(),
    )))
    .unwrap();

    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        capability_registry,
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // Eager capabilities start with the host.
    assert!(
        *started.lock().unwrap(),
        "eager capability starts at host start"
    );

    // Registration alone keeps the capability's tools off the model
    // surface: they are catalog-visible but Available.
    let tools = host.registry().tool_provider().unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(
        !names.contains(&"demo.demo".to_string()),
        "unloaded capability tools must not be on the surface"
    );
    let catalog = dispatcher.catalog();
    let row = catalog
        .iter()
        .find(|entry| entry.name == "demo.demo")
        .expect("capability tools are discoverable in the catalog");
    assert_eq!(row.state, ToolLifecycle::Available);
    assert_eq!(row.owner, "demo");

    // Explicit load puts the capability's tools on the surface.
    dispatcher.load_tool("demo.demo").unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(names.contains(&"demo.demo".to_string()));

    // A call routed by name reaches the capability.
    let output = execute(tools, "demo.demo").await;
    assert!(output.ok);
    assert_eq!(output.model_content, "demo handled demo.demo");

    host.stop().await.unwrap();
}

#[tokio::test]
async fn capabilities_can_be_registered_mid_run_and_lazy_start_on_use() {
    let mut host = ModuleHost::new();
    let capability_registry = host.capability_registry();
    let dispatcher = Arc::new(CapabilityAwareDispatcher::new(
        Arc::new(StubTools),
        capability_registry,
    ));
    host.add_module(Arc::new(ToolModule::new(dispatcher.clone())))
        .unwrap();
    host.start().await.unwrap();

    // Mid-run registration: the tool is discoverable but not on the
    // surface until it is loaded.
    let started = Arc::new(Mutex::new(false));
    host.register_capability(Arc::new(DemoCapability::new(
        "late",
        CapabilityLifecycle::Lazy,
        started.clone(),
    )))
    .unwrap();

    let tools = host.registry().tool_provider().unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(
        !names.contains(&"late.demo".to_string()),
        "unloaded capability tools must not be on the surface"
    );
    assert!(
        !*started.lock().unwrap(),
        "a lazy capability is not started at registration"
    );

    // Load it, then the first invocation starts it (lazy lifecycle).
    dispatcher.load_tool("late.demo").unwrap();
    let names: Vec<String> = tools.specs().iter().map(|spec| spec.name.clone()).collect();
    assert!(names.contains(&"late.demo".to_string()));
    let output = execute(tools, "late.demo").await;
    assert!(output.ok);
    assert!(
        *started.lock().unwrap(),
        "lazy capability starts on first use"
    );

    host.stop().await.unwrap();
}

#[tokio::test]
async fn capability_dependencies_are_validated() {
    let host = ModuleHost::new();
    let orphan = DemoCapability::with_dependency("orphan", "missing-capability");
    let error = host
        .register_capability(Arc::new(orphan))
        .expect_err("a capability with an unmet requirement must be rejected");
    assert!(
        error.to_string().contains("requires"),
        "the error must name the missing requirement: {error}"
    );

    // Duplicate ids are rejected too.
    let started = Arc::new(Mutex::new(false));
    let first = DemoCapability::new("dup", CapabilityLifecycle::Eager, started.clone());
    let second = DemoCapability::new("dup", CapabilityLifecycle::Eager, started);
    host.register_capability(Arc::new(first)).unwrap();
    let error = host
        .register_capability(Arc::new(second))
        .expect_err("duplicate capability ids must be rejected");
    assert!(error.to_string().contains("already registered"), "{error}");
}

#[tokio::test]
async fn capability_authority_is_derived_and_validated_at_registration() {
    let host = ModuleHost::new();

    // An id outside the conservative grammar is a path/route injection
    // risk and is refused before anything else.
    let bad_id = DemoCapability::with_authority("../escape", &["workspace:read"], Vec::new());
    let error = host
        .register_capability(Arc::new(bad_id))
        .expect_err("a path-unsafe id must be rejected");
    assert!(
        error.to_string().contains("capability id"),
        "the refusal must name the id rule: {error}"
    );

    // Self-declared ReadOnly on a workspace-write capability: ReadOnly
    // auto-allows at the approval gate, so a mutating capability must
    // never self-declare it — the risk is derived from the authority.
    let write_tool = DemoCapability::with_authority("write-tool", &["workspace:write"], Vec::new());
    let error = host
        .register_capability(Arc::new(write_tool))
        .expect_err("a write-permissioned capability must not self-declare ReadOnly");
    assert!(
        error.to_string().contains("ReadOnly"),
        "the refusal must name the self-declared risk: {error}"
    );

    // A tool whose risk exceeds its grant is refused: WorkspaceWrite
    // without the permission, ProcessExecution without the permission.
    let over_granted = DemoCapability::with_authority(
        "over-granted",
        &["workspace:read"],
        vec![ToolSpec {
            name: "over-granted.run".into(),
            description: "writes".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: Vec::new(),
        }],
    );
    let error = host
        .register_capability(Arc::new(over_granted))
        .expect_err("a tool risk may not exceed the declared grant");
    assert!(
        error.to_string().contains("workspace:write"),
        "the refusal must name the missing grant: {error}"
    );

    // A process capability may declare workspace:write now that the wire
    // effect broker exists: the child stages structured wire effects and
    // the adapter commits them through the confined workspace handle behind
    // the generation fence — the child itself never writes.
    let process_write = DemoCapability::with_authority(
        "proc-write",
        &["workspace:write"],
        vec![ToolSpec {
            name: "proc-write.run".into(),
            description: "writes".into(),
            input_schema: json!({"type": "object"}),
            risk: ToolRisk::WorkspaceWrite,
            output_budget: None,
            roles: Vec::new(),
        }],
    );
    let process_write = {
        let mut capability = process_write;
        capability.manifest.transport = CapabilityTransport::Process {
            program: "x".into(),
        };
        capability
    };
    host.register_capability(Arc::new(process_write))
        .expect("a process capability may declare workspace:write through the wire effect broker");

    // A read-only process capability is fine: read authority, ReadOnly
    // tool, no broker needed.
    let process_read = DemoCapability::with_authority("proc-read", &["workspace:read"], Vec::new());
    let process_read = {
        let mut capability = process_read;
        capability.manifest.transport = CapabilityTransport::Process {
            program: "x".into(),
        };
        capability
    };
    host.register_capability(Arc::new(process_read))
        .expect("a read-only process capability is allowed");
}

#[tokio::test]
async fn missing_capability_lookup_fails_with_a_clear_error() {
    let host = ModuleHost::new();
    match host.registry().context_service() {
        Ok(_) => panic!("a missing capability must fail"),
        Err(error) => assert!(
            error.to_string().contains("context-service"),
            "the error must name the missing capability, got: {error}"
        ),
    }
    // Module claims are empty for an unregistered id.
    assert!(host.registry().claims(APPROVAL_POLICY).is_none());
}

#[tokio::test]
async fn external_capabilities_start_experimental_regardless_of_declared_status() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();

    // An out-of-process capability declares itself Stable; the platform must
    // not let the LLM promote its own module.
    registry
        .register(Arc::new(DemoCapability::declared_stable(
            "ext-llm-module",
            CapabilityTransport::Process {
                program: "plugin".into(),
            },
        )))
        .unwrap();
    assert_eq!(
        registry.status("ext-llm-module"),
        Some(CapabilityStatus::Experimental),
        "external capabilities enter at the bottom of the maturity ladder"
    );
    assert_eq!(
        registry.activation("ext-llm-module"),
        Some(CapabilityActivation::Disabled),
        "external capabilities enter disabled; enabling is an operator action"
    );

    // The catalog reports the effective status and activation, not the
    // declaration.
    let entry = registry
        .catalog()
        .into_iter()
        .find(|entry| entry.id == "ext-llm-module")
        .expect("registered capability must appear in the catalog");
    assert_eq!(entry.status, CapabilityStatus::Experimental);
    assert_eq!(entry.activation, CapabilityActivation::Disabled);
    assert_eq!(entry.tools, vec!["ext-llm-module.demo".to_string()]);

    // A disabled capability cannot put its tools on the model surface.
    let error = registry
        .load_tool("ext-llm-module.demo")
        .expect_err("loading a disabled capability's tools must fail");
    assert!(
        error.to_string().contains("disabled"),
        "the error must name the activation: {error}"
    );
}

#[tokio::test]
async fn trusted_builtin_capabilities_keep_their_declared_status() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    registry
        .register(Arc::new(DemoCapability::declared_stable(
            "trusted-core",
            CapabilityTransport::Builtin,
        )))
        .unwrap();
    assert_eq!(
        registry.status("trusted-core"),
        Some(CapabilityStatus::Stable),
        "the trusted core declares its own maturity"
    );
    assert_eq!(
        registry.activation("trusted-core"),
        Some(CapabilityActivation::Enabled),
        "the trusted in-process core is usable immediately"
    );
}

#[tokio::test]
async fn capabilities_cannot_shadow_reserved_core_tool_names() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    // The dispatcher claims the builtin catalog plus the control tools.
    let _dispatcher = CapabilityAwareDispatcher::new(Arc::new(StubTools), registry.clone());

    // Declaring a builtin tool name must be rejected at registration: the
    // route would otherwise be hijackable by declaration.
    let hijack = DemoCapability::with_tool_names(
        "shadow-builtin",
        &["fs.read"],
        CapabilityTransport::Builtin,
    );
    let error = registry
        .register(Arc::new(hijack))
        .expect_err("shadowing a builtin tool name must be rejected");
    assert!(
        error.to_string().contains("reserved"),
        "the error must name the reservation: {error}"
    );

    // Control tools are reserved too.
    let control = DemoCapability::with_tool_names(
        "shadow-control",
        &[agent_contracts::CAPABILITY_MANAGE],
        CapabilityTransport::Builtin,
    );
    let error = registry
        .register(Arc::new(control))
        .expect_err("shadowing a control tool must be rejected");
    assert!(error.to_string().contains("reserved"), "{error}");
}

#[tokio::test]
async fn capabilities_cannot_duplicate_each_others_tool_names() {
    let host = ModuleHost::new();
    let registry = host.capability_registry();
    registry
        .register(Arc::new(DemoCapability::with_tool_names(
            "first",
            &["shared.tool"],
            CapabilityTransport::Builtin,
        )))
        .unwrap();
    let error = registry
        .register(Arc::new(DemoCapability::with_tool_names(
            "second",
            &["shared.tool"],
            CapabilityTransport::Builtin,
        )))
        .expect_err("a second owner of the same tool name must be rejected");
    assert!(
        error.to_string().contains("already owned"),
        "the error must name the existing owner: {error}"
    );
}
