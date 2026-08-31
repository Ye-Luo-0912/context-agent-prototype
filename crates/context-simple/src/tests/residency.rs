use agent_contracts::{
    ContextAction, ContextEngine, ContextIngress, ContextItemId, ContextKind,
    ContextMaintenanceTrigger, ContextRetention, ContextScope, ScopeKind, ScopeState,
    SemanticState,
};

use crate::engine::{SimpleContextConfig, SimpleContextEngine};

use super::harness::*;

#[tokio::test]
async fn gc_externalizes_overflow_and_recalls_via_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        // Keep only the newest file body as a residency root so an older
        // live body can overflow into the store (same-path fs.read would
        // supersede instead; stamped shell logs must not auto-recall).
        recent_file_bodies: 1,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let files = ["AuthService.rs", "CacheStore.rs", "TokenCache.rs"];
    for (i, path) in files.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: fs_read_touching(
                    &format!("step-{i}"),
                    path,
                    &format!("     1 | step {i}: fix {path}"),
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    // TokenCache.rs stays as the latest-file root. The other two bodies
    // evict; the buffer holds one and the oldest overflows to the store.
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.externalized, 1,
        "buffer overflow must externalize to the store: {report:?}"
    );
    let stored = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(stored, report.externalized, "the store files must exist");

    // Hot entities belonging to an externalized item recall it: the store
    // read happens in the IO phase and the item re-enters the heap.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "continue on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.reactivated >= 1,
        "a hot externalized item must be recalled: {report:?}"
    );
    assert!(
        report
            .reactivations
            .iter()
            .any(|r| r.reason.contains("recalled from the context store")),
        "the recall must be explainable: {:?}",
        report.reactivations
    );
    // Recalled content is resident again, so its blobs are deleted only
    // *after* the commit landed — every formal blob has exactly one owner,
    // and a crash between commit and delete leaves an orphan the startup
    // reconcile re-owns. One owner, one file.
    let stored = std::fs::read_dir(dir.path()).unwrap().count();
    assert_eq!(
        stored, 0,
        "recalled blobs must be removed once their content is resident"
    );
}

/// A tampered store blob must never reach a consumer: `fetch` turns the
/// ownership-checksum mismatch into a hard read failure instead of serving
/// substituted content.
#[tokio::test]
async fn fetch_rejects_a_tampered_blob() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        recent_file_bodies: 1,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let files = ["AuthService.rs", "CacheStore.rs", "TokenCache.rs"];
    for (i, path) in files.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: fs_read_touching(
                    &format!("step-{i}"),
                    path,
                    &format!("     1 | step {i}: fix {path}"),
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.externalized, 1,
        "overflow must externalize: {report:?}"
    );
    let externalized_id = report.externalized_ids[0];

    // Valid JSON, same id, changed body: the checksum mismatch must fail
    // the authoritative read, not substitute the bytes.
    std::fs::write(
        dir.path().join(format!("{externalized_id}.json")),
        serde_json::to_vec(&agent_contracts::ContextItem {
            content: "substituted body".into(),
            ..engine
                .fetch_external(externalized_id)
                .await
                .expect("fetch must still resolve the pre-tamper entry")
                .expect("the item must still be external")
        })
        .unwrap(),
    )
    .unwrap();

    let failure = engine.fetch_external(externalized_id).await.unwrap_err();
    assert!(
        failure.to_string().contains("corrupt"),
        "the tampered read must surface as a corruption failure, got: {failure}"
    );
}

/// A tampered store blob must never re-enter the resident heap: the GC
/// recall read verifies the ownership checksum, so a substituted body is
/// left in the store (for the reconcile to quarantine) instead of being
/// reactivated under the original identity.
#[tokio::test]
async fn recall_rejects_a_tampered_blob() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        recent_file_bodies: 1,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let files = ["AuthService.rs", "CacheStore.rs", "TokenCache.rs"];
    for (i, path) in files.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: fs_read_touching(
                    &format!("step-{i}"),
                    path,
                    &format!("     1 | step {i}: fix {path}"),
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.externalized, 1,
        "first GC must externalize: {report:?}"
    );
    let externalized_id = report.externalized_ids[0];
    let entry = engine
        .search_external(agent_contracts::ContextSearchQuery {
            query: "AuthService".into(),
            kind: None,
            scope: None,
            task_id: None,
            label: None,
            limit: 16,
        })
        .await
        .unwrap();
    assert!(
        entry.iter().any(|hit| hit.item_id == externalized_id),
        "the externalized item must be searchable before the tamper"
    );

    // Tamper the blob under the same id, then make its entity hot again.
    std::fs::write(
        dir.path().join(format!("{externalized_id}.json")),
        serde_json::to_vec(&agent_contracts::ContextItem {
            content: "substituted body".into(),
            ..engine
                .fetch_external(externalized_id)
                .await
                .expect("fetch must still resolve the pre-tamper entry")
                .expect("the item must still be external")
        })
        .unwrap(),
    )
    .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "continue on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        !report
            .reactivations
            .iter()
            .any(|r| r.reason.contains("recalled from the context store")),
        "the tampered blob must not be recalled: {report:?}"
    );
    assert!(
        dir.path().join(format!("{externalized_id}.json")).exists(),
        "the rejected blob stays in the store for the reconcile to quarantine"
    );
}

/// The external store is a fidelity boundary: `fetch(ref)` must recover the
/// exact content that was externalized, not a summary or a truncated copy.
#[tokio::test]
async fn fetch_external_recovers_the_exact_original_content() {
    let dir = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(dir.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    let contents = ["step 0: fix AuthService.rs", "step 1: fix AuthService.rs"];
    for (i, content) in contents.iter().enumerate() {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: observation_touching(
                    &format!("step-{i}"),
                    true,
                    content,
                    Some("AuthService.rs"),
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(
        report.externalized >= 1,
        "buffer overflow must externalize: {report:?}"
    );
    assert_eq!(
        report.externalized_ids.len(),
        report.externalized,
        "found-after-forgotten 必须能按 id 对齐本次外置"
    );

    // Find one externalized ref through the retrieval surface, then pull
    // its full content back across the store boundary.
    let refs = engine
        .search_external(agent_contracts::ContextSearchQuery {
            query: "AuthService".into(),
            kind: Some(ContextKind::ToolObservation),
            scope: None,
            task_id: None,
            label: None,
            limit: 16,
        })
        .await
        .unwrap();
    assert!(
        !refs.is_empty(),
        "the search must surface the externalized refs"
    );
    let fetched = engine
        .fetch_external(refs[0].item_id)
        .await
        .unwrap()
        .expect("fetch must return the externalized item");
    assert_eq!(
        fetched.id, refs[0].item_id,
        "fetch must return the item the ref points at"
    );
    assert_eq!(
        fetched.kind,
        ContextKind::ToolObservation,
        "the recovered item keeps its kind"
    );
    assert!(
        contents.contains(&fetched.content.as_str()),
        "fetch must recover the exact original content, got: {:?}",
        fetched.content
    );
}

/// The context store is confined: with an explicit store dir every write
/// lands under it, and the default fallback is an OS temp dir — never a
/// CWD-relative path, so a misconfigured runtime cannot scatter externalized
/// content into the launch directory.
#[tokio::test]
async fn context_store_never_writes_outside_the_state_directory() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("context-store");
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 1,
        gc_reactivate_per_pass: 8,
        context_store_dir: Some(store.clone()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "service layer").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs".into(),
        })
        .await
        .unwrap();
    for i in 0..3 {
        engine
            .ingest(ContextIngress::ToolObservation {
                facts: None,
                output: observation_touching(
                    &format!("step-{i}"),
                    true,
                    &format!("step {i}: fix AuthService.rs"),
                    Some("AuthService.rs"),
                ),
                scope_id: None,
            })
            .await
            .unwrap();
    }
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert!(report.externalized >= 1, "overflow must externalize");

    // Every store file is inside the configured directory and nothing else
    // was created nearby.
    let files: Vec<_> = std::fs::read_dir(&store).unwrap().collect();
    assert_eq!(
        files.len(),
        report.externalized,
        "all store files land in the configured dir"
    );
    for file in files {
        let path = file.unwrap().path();
        assert!(path.starts_with(&store), "store file escaped: {path:?}");
        assert_eq!(
            path.extension().and_then(|e| e.to_str()),
            Some("json"),
            "store files are the item payloads"
        );
    }
    // The old CWD-relative fallback must never appear.
    let legacy = std::env::current_dir()
        .unwrap()
        .join(".focus-agent")
        .join("context-store");
    assert!(
        !legacy.exists(),
        "no store may be created under the CWD: {}",
        legacy.display()
    );

    // The default fallback is an OS temp dir, never CWD-derived.
    let default_dir = crate::store::store_dir(&SimpleContextConfig::default());
    assert!(
        default_dir.starts_with(std::env::temp_dir()),
        "the default store must live under the OS temp dir, got: {default_dir:?}"
    );
    assert!(
        !default_dir.starts_with(std::env::current_dir().unwrap()),
        "the default store must never be CWD-relative: {default_dir:?}"
    );
}

/// Ordinary dialogue of the *open* focus episode ages out of the working
/// set: related messages share tokens, so the score floor keeps them
/// Active and the focus-scope root would otherwise accumulate every turn
/// of a long episode (a 500-turn episode would hold ~500 messages even
/// though no rotation fires). After the staleness window (ttl x 4) the
/// dialogue leaves the heap into the reversible buffer and is not bounced
/// back by its own high score, so the resident working set stays bounded
/// without relying on episode rotation.
#[tokio::test]
async fn open_focus_ordinary_dialogue_ages_out_without_rotation() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "keep working on the auth service").await;

    // Related messages across several staleness windows (default ttl x 4 =
    // 20 turns), with periodic GC. The resident heap must not accumulate
    // every message: aged ordinary dialogue leaves Resident even though
    // its score floor keeps it Active.
    let mut max_resident = 0usize;
    for turn in 1..=120u64 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("keep working on the auth cache in round {turn}"),
            })
            .await
            .unwrap();
        tool_observation(
            &engine,
            &turn.to_string(),
            &format!("patched Item{}", turn % 7),
        )
        .await;
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        if turn % 10 == 0 {
            engine.gc().await.unwrap();
            let resident = engine.state.lock().await.items.len();
            max_resident = max_resident.max(resident);
        }
    }
    assert!(
        max_resident < 60,
        "open-episode ordinary dialogue must age out, peak resident was {max_resident}"
    );

    // The bound comes from the aging rule, not from episode rotation: the
    // focus scope stays open the whole run (120 turns < the 500-turn
    // budget), so no closed-scope eviction ever fired.
    let state = engine.state.lock().await;
    let open_focus_scopes = state
        .scopes
        .iter()
        .filter(|s| s.kind == ScopeKind::Focus && s.state == ScopeState::Active)
        .count();
    assert_eq!(
        open_focus_scopes, 1,
        "the episode must stay open (no rotation); the bound must come from aging"
    );
}

/// A live item that moved to the warm buffer shares the same lifecycle
/// clock as a resident one: an ephemeral observation evicted to the
/// reversible buffer is tombstoned when its TTL passes, so it cannot be
/// reactivated forever from a location the residency pass no longer
/// visits.
#[tokio::test]
async fn warm_ephemeral_item_is_tombstoned_by_ttl_not_only_resident() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "fix AuthService.rs").await;

    // A consumed ephemeral tool observation leaves the heap at GC time.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "inspect AuthService.rs".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "read AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    let obs_id = {
        let state = engine.state.lock().await;
        state
            .eviction_buffer
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .map(|item| item.id)
    };
    let obs_id = obs_id.expect("consumed observation must sit in the warm buffer");
    {
        let state = engine.state.lock().await;
        let item = state
            .eviction_buffer
            .iter()
            .find(|item| item.id == obs_id)
            .expect("warm item");
        assert!(item.semantic.is_live(), "a warm item starts live");
    }

    // Advance past the ephemeral TTL (default 5 turns) with unrelated
    // user turns; the warm buffer must tombstone it there.
    for turn in 1..=6u64 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("keep working in round {turn}"),
            })
            .await
            .unwrap();
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
    }
    {
        let state = engine.state.lock().await;
        let item = state
            .eviction_buffer
            .iter()
            .find(|item| item.id == obs_id)
            .expect("warm item");
        assert_eq!(
            item.semantic,
            SemanticState::Tombstoned,
            "the ephemeral TTL must reach the warm buffer"
        );
    }

    // A tombstoned warm item is never reactivated, even when its entity
    // is hot again.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "read AuthService.rs again".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "2", "touched AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            !state.items.iter().any(|item| item.id == obs_id),
            "a tombstoned warm item must never reactivate"
        );
        let item = state
            .eviction_buffer
            .iter()
            .find(|item| item.id == obs_id)
            .expect("warm item stays in the buffer");
        assert_eq!(item.semantic, SemanticState::Tombstoned);
    }
}

/// A Working item that aged into the warm buffer shares the same
/// staleness clock: after ttl x 4 turns it is tombstoned there too, so
/// the reversible buffer cannot keep stale ordinary dialogue
/// reactivatable forever.
#[tokio::test]
async fn warm_working_item_is_tombstoned_by_staleness() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    open_focus(&engine, "keep working on the auth service").await;

    // Related messages with no entities age out of the open episode
    // (ordinary-dialogue rule) into the warm buffer; the buffer's
    // staleness clock then tombstones them.
    for turn in 1..=30u64 {
        engine
            .ingest(ContextIngress::UserMessage {
                content: format!("keep working on the auth cache in round {turn}"),
            })
            .await
            .unwrap();
        tool_observation(
            &engine,
            &turn.to_string(),
            &format!("patched Item{}", turn % 7),
        )
        .await;
        engine
            .maintain(ContextMaintenanceTrigger::AfterModel)
            .await
            .unwrap();
        if turn % 5 == 0 {
            engine.gc().await.unwrap();
        }
    }

    // One more pass so messages the final GC just moved into the buffer
    // get their staleness check there.
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    // At turn 30 every warm UserMessage older than 20 turns must be
    // tombstoned — the buffer must not keep it reactivatable forever.
    let state = engine.state.lock().await;
    let stale_in_buffer: Vec<_> = state
        .eviction_buffer
        .iter()
        .filter(|item| item.kind == ContextKind::UserMessage)
        .filter(|item| state.turn.saturating_sub(item.created_turn) > 20)
        .collect();
    assert!(
        !stale_in_buffer.is_empty(),
        "expected aged ordinary dialogue in the warm buffer"
    );
    assert!(
        stale_in_buffer.iter().all(|item| !item.semantic.is_live()),
        "stale warm dialogue must be tombstoned, not kept reactivatable: {:?}",
        stale_in_buffer
            .iter()
            .map(|item| (item.created_turn, item.semantic))
            .collect::<Vec<_>>()
    );
}

/// A terminal semantic transition (supersession) must reach the target
/// wherever its body currently sits. A decision externalized to the store
/// (Cold) and one sitting in the warm buffer are still the same decisions:
/// a later decision on the same entities supersedes them.
#[tokio::test]
async fn supersession_reaches_warm_and_stored_decisions() {
    let store = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_buffer_capacity: 4,
        // First GC pass evicts any unmarked Cooling/Archived item, so the
        // switched-away task's records leave Resident without a long TTL.
        gc_max_generation: 0,
        context_store_dir: Some(store.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "auth work").await;
    // A1 (AuthService.rs) is the oldest decision: it overflows to Cold.
    // A3 (CacheStore.rs) is newer: it stays Warm in the buffer.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "touched AuthService.rs").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use CacheStore.rs for caching".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "2", "edited CacheStore.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let (a1_id, a3_id) = {
        let state = engine.state.lock().await;
        (
            state
                .items
                .iter()
                .find(|item| item.content.contains("use AuthService.rs"))
                .expect("decision A1")
                .id,
            state
                .items
                .iter()
                .find(|item| item.content.contains("use CacheStore.rs"))
                .expect("decision A3")
                .id,
        )
    };

    // Switch to task B: A's scopes suspend, A1/A3 cool out of the working
    // set. GC evicts both; the 4-item buffer overflows the oldest (A1) to
    // the store while A3 stays Warm.
    open_focus(&engine, "cache work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "investigate the cache miss pattern".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "3", "traced the cache").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let gc = engine.gc().await.unwrap();
    assert!(gc.externalized >= 1, "A1 must reach the store: {gc:?}");
    {
        let state = engine.state.lock().await;
        assert!(state.external.get(a1_id).is_some(), "A1 is a Cold entry");
        assert!(
            state.eviction_buffer.iter().any(|item| item.id == a3_id),
            "A3 stays Warm in the buffer"
        );
    }

    // A later decision on the same entities supersedes each, wherever it
    // sits. The maintain pass applies the queued terminal transitions.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for the cache layer".into(),
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use CacheStore.rs for the read path".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let a1 = state.external.get(a1_id).expect("stored decision entry");
    assert!(
        a1.semantic.is_dead(),
        "the stored decision must be superseded, got {:?}",
        a1.semantic
    );
    let a3 = state
        .eviction_buffer
        .iter()
        .find(|item| item.id == a3_id)
        .expect("warm decision");
    assert!(
        a3.semantic.is_dead(),
        "the warm decision must be superseded, got {:?}",
        a3.semantic
    );
}

/// Error verification must reach an error that left Resident. A
/// successful result on the same entities verifies a Warm error as readily
/// as a resident one.
#[tokio::test]
async fn verification_reaches_warm_errors() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_max_generation: 0,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "auth work").await;
    // A failing tool result persists as a Working Error in task A.
    engine
        .ingest(ContextIngress::UserMessage {
            content: "debug the auth failure".into(),
        })
        .await
        .unwrap();
    failed_observation(&engine, "1", "error in AuthService.rs:42").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let error_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .expect("error item")
            .id
    };

    // Task B takes over; A's error cools and is evicted to the buffer.
    open_focus(&engine, "cache work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "look at the cache".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "2", "examined the cache").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.iter().any(|item| item.id == error_id),
            "the error must be Warm for this test"
        );
    }

    // A successful result on the same entities verifies the Warm error.
    tool_observation(&engine, "3", "fixed AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let error = state
        .eviction_buffer
        .iter()
        .find(|item| item.id == error_id)
        .expect("warm error");
    assert!(
        error.semantic.is_dead(),
        "the warm error must be verified fixed, got {:?}",
        error.semantic
    );
}

/// A recurring failure supersedes the earlier error wherever it sits — a
/// Warm error is superseded by the next failure on the same site.
#[tokio::test]
async fn recurrence_supersedes_warm_errors() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_max_generation: 0,
        ..SimpleContextConfig::default()
    });
    open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "debug the auth failure".into(),
        })
        .await
        .unwrap();
    failed_observation(&engine, "1", "error in AuthService.rs:42").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let error_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Error)
            .expect("error item")
            .id
    };

    open_focus(&engine, "cache work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "look at the cache".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "2", "examined the cache").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state.eviction_buffer.iter().any(|item| item.id == error_id),
            "the error must be Warm for this test"
        );
    }

    // Same failure site again, in task B: recurrence supersedes the Warm
    // error from task A. Identical content keeps the entity signature
    // (including the line number) identical, as a real recurring failure
    // on the same site would.
    failed_observation(&engine, "3", "error in AuthService.rs:42").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let error = state
        .eviction_buffer
        .iter()
        .find(|item| item.id == error_id)
        .expect("warm error");
    assert!(
        error.semantic.is_dead(),
        "the recurring failure must supersede the warm error, got {:?}",
        error.semantic
    );
}

/// Completing a task clears model protections (keep_alive / lease)
/// in every body location, so a completed task cannot keep rooting items
/// through a warm-buffer record.
#[tokio::test]
async fn completed_task_clears_protections_in_every_residency() {
    let engine = SimpleContextEngine::new(SimpleContextConfig::default());
    let task_id = open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "use AuthService.rs for login".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "touched AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();

    // Protect one resident item (keep_alive + lease), then move it into
    // the warm buffer (an old-checkpoint path a normal GC would never
    // produce, because protected items are roots).
    let protected_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::ToolObservation)
            .expect("tool observation")
            .id
    };
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::GcHint {
                item_id: protected_id,
                keep_alive: true,
            },
        })
        .await
        .unwrap();
    engine
        .ingest(ContextIngress::ContextDirective {
            action: ContextAction::Lease {
                item_id: protected_id,
                turns: 32,
            },
        })
        .await
        .unwrap();
    let warm_id = {
        let mut state = engine.state.lock().await;
        let items = state.items.take_all();
        let mut protected = None;
        let mut rest = Vec::new();
        for item in items {
            if item.id == protected_id {
                protected = Some(item);
            } else {
                rest.push(item);
            }
        }
        state.items.replace_all(rest);
        let protected = protected.expect("the protected item");
        assert!(protected.keep_alive && protected.lease_until_turn.is_some());
        let id = protected.id;
        state.eviction_buffer.push(protected);
        id
    };

    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_id),
            summary: "auth work done".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let warm = state
        .eviction_buffer
        .iter()
        .find(|item| item.id == warm_id)
        .expect("warm protected item");
    assert!(
        !warm.keep_alive && warm.lease_until_turn.is_none(),
        "completed task must clear protections in the warm buffer, got keep_alive={} lease={:?}",
        warm.keep_alive,
        warm.lease_until_turn
    );
}

/// A completed task clears model protections in the external map too: the
/// entry captures keep_alive/lease at externalize time, and the task close
/// must drop them in every body location so a finished task cannot keep
/// rooting its records through a stored reference.
#[tokio::test]
async fn completed_task_clears_protections_in_external_entries() {
    let store = tempfile::tempdir().unwrap();
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        context_store_dir: Some(store.path().to_path_buf()),
        ..SimpleContextConfig::default()
    });
    let task_id = open_focus(&engine, "maintain the auth service").await;

    // An external entry for the focused task carrying model protections.
    let protected_id = {
        let mut state = engine.state.lock().await;
        let mut item = crate::item::make_item(
            &state,
            &engine.config,
            "AuthService.rs decision: keep the token cache".into(),
            ContextKind::Note,
            ContextScope::Task,
            ContextRetention::Durable,
            0.5,
            None,
        );
        item.id = ContextItemId::new();
        item.task_id = Some(task_id);
        item.keep_alive = true;
        item.lease_until_turn = Some(99);
        item.entities = crate::index::entity::extract_entities(&item.content);
        let reference = crate::store::externalize(store.path(), &item).unwrap();
        state.external.push(crate::store::to_external_entry(
            &item, reference, 1, 1, None,
        ));
        item.id
    };
    {
        let state = engine.state.lock().await;
        let entry = state
            .external
            .get(protected_id)
            .expect("one external entry");
        assert!(
            entry.keep_alive && entry.lease_until_turn == Some(99),
            "the entry captures the protections at externalize time"
        );
    }

    // Completing the task clears the protections in the external map.
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_id),
            summary: "auth service maintained".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();

    let state = engine.state.lock().await;
    let entry = state
        .external
        .get(protected_id)
        .expect("the entry stays in the map");
    assert!(
        !entry.keep_alive && entry.lease_until_turn.is_none(),
        "completed task must clear protections in the external map, got keep_alive={} lease={:?}",
        entry.keep_alive,
        entry.lease_until_turn
    );
}

/// Automatic hot-entity recall of a completed task's records is forbidden
/// without an explicit reason. The hot set alone must not bring finished
/// work back as current truth.
#[tokio::test]
async fn completed_task_blocks_automatic_hot_recall() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_max_generation: 0,
        ..SimpleContextConfig::default()
    });
    let task_a = open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "fix AuthService.rs".into(),
        })
        .await
        .unwrap();
    tool_observation(&engine, "1", "touched AuthService.rs").await;
    engine
        .maintain(ContextMaintenanceTrigger::AfterModel)
        .await
        .unwrap();
    let message_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::UserMessage)
            .expect("task A message")
            .id
    };

    // Complete task A: its scopes close, the working set is evicted, GC
    // moves the archived dialogue to the warm buffer.
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_a),
            summary: "auth fixed".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();
    engine.gc().await.unwrap();
    {
        let state = engine.state.lock().await;
        assert!(
            state
                .eviction_buffer
                .iter()
                .any(|item| item.id == message_id),
            "the completed task's dialogue must be Warm for this test"
        );
    }

    // Task B makes the same entity hot. GC must NOT auto-recall the
    // completed task's record (no explicit reason); an active task's
    // evicted record would be recalled here.
    open_focus(&engine, "auth follow-up").await;
    engine
        .ingest(ContextIngress::UserMessage {
            content: "work on AuthService.rs again".into(),
        })
        .await
        .unwrap();
    let report = engine.gc().await.unwrap();
    assert_eq!(
        report.reactivated, 0,
        "a completed task's record must not auto-return on hot entities, got {report:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|item| item.id == message_id),
        "completed-task dialogue must stay out of the resident heap"
    );
}

/// A completed task's summary is a *storage* root, not a residency root:
/// after completion and one full GC pass it leaves the resident heap (its
/// durable retention keeps it protected in the reversible buffer or the
/// store), so the heap cannot grow with every completed task.
#[tokio::test]
async fn completed_task_summary_leaves_the_resident_heap_but_stays_durable() {
    let engine = SimpleContextEngine::new(SimpleContextConfig {
        gc_max_generation: 0,
        ..SimpleContextConfig::default()
    });
    let task_a = open_focus(&engine, "auth work").await;
    engine
        .ingest(ContextIngress::TaskCompleted {
            task_id: Some(task_a),
            summary: "auth fixed".into(),
        })
        .await
        .unwrap();
    engine
        .maintain(ContextMaintenanceTrigger::TaskCompleted)
        .await
        .unwrap();

    let summary_id = {
        let state = engine.state.lock().await;
        state
            .items
            .iter()
            .find(|item| item.kind == ContextKind::Summary)
            .expect("completion summary")
            .id
    };

    // One full GC pass must not keep the finished task's summary resident.
    let report = engine.gc().await.unwrap();
    assert!(
        report.evicted >= 1,
        "the completed task's records must leave the heap, got {report:?}"
    );
    let state = engine.state.lock().await;
    assert!(
        !state.items.iter().any(|item| item.id == summary_id),
        "the summary must leave the resident heap"
    );
    let in_buffer = state
        .eviction_buffer
        .iter()
        .any(|item| item.id == summary_id);
    let in_store = state
        .external
        .iter()
        .any(|entry| entry.item_id == summary_id);
    assert!(
        in_buffer || in_store,
        "the durable summary must stay recallable from the buffer or the store"
    );
}
