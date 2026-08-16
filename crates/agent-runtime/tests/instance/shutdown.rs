use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_contracts::RuntimeEvent;
use agent_runtime::{ModuleHost, RuntimeInstance};

use crate::harness::*;

#[tokio::test]
async fn shutdown_stops_modules_and_joins_the_actor() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(LifecycleModule {
        log: log.clone(),
        fail_stop: false,
    }))
    .unwrap();
    host.start().await.unwrap();

    let services = services();
    let instance = RuntimeInstance::spawn(host, services);
    // Subscribe through the handle: the actor's kernel is derived inside
    // the spawn seam, so the handle is the one live event source.
    let mut events = instance.handle().subscribe();
    instance.start().await.unwrap();
    instance.shutdown().await.unwrap();

    let order = log.lock().unwrap();
    assert_eq!(
        &order[..],
        &["start", "stop"],
        "the module lifecycle must bracket the run"
    );
    drop(order);

    let mut run_completed = false;
    while let Ok(envelope) = events.try_recv() {
        if matches!(envelope.event, RuntimeEvent::RunCompleted) {
            run_completed = true;
        }
    }
    assert!(
        run_completed,
        "shutdown must flush the kernel (RunCompleted) before stopping modules"
    );
}

#[tokio::test]
async fn shutdown_aggregates_module_stop_errors() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(LifecycleModule {
        log: log.clone(),
        fail_stop: true,
    }))
    .unwrap();
    host.start().await.unwrap();

    let instance = RuntimeInstance::spawn(host, services());
    instance.start().await.unwrap();
    let error = instance.shutdown().await.unwrap_err();

    let message = error.to_string();
    assert!(
        message.contains("module host stop") && message.contains("module stop failed"),
        "shutdown must aggregate the module failure, got: {message}"
    );
    // The actor task still joined even though the module stop failed.
    let order = log.lock().unwrap();
    assert_eq!(&order[..], &["start", "stop"]);
}

#[tokio::test]
async fn shutdown_with_no_turn_is_a_clean_noop_path() {
    let mut host = ModuleHost::new();
    host.add_module(Arc::new(LifecycleModule {
        log: Arc::new(Mutex::new(Vec::new())),
        fail_stop: false,
    }))
    .unwrap();
    host.start().await.unwrap();
    let instance = RuntimeInstance::spawn(host, services());
    // Never started the actor; shutdown must still complete within a bounded
    // time (cancel is a no-op, stop is a no-op, host stops, task joins).
    let result = tokio::time::timeout(Duration::from_secs(2), instance.shutdown())
        .await
        .expect("shutdown must not hang");
    assert!(result.is_ok());
}
