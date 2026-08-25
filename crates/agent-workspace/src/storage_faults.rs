//! Portable storage-full fault injection seam (feature `test-faults`).
//!
//! Real disk exhaustion cannot be produced deterministically or portably
//! in CI, so the durability-relevant write sites in this crate consult an
//! armed [`StorageFaultPlan`] and fail exactly like a full volume would.
//! The plan is inert until a fixture explicitly arms it through
//! [`crate::Workspace::arm_storage_faults`]; default builds do not compile
//! that entry point, so no production surface can suppress writes.
//!
//! The three fault points mirror the write ordering of a Core-managed
//! mutation: the synced authority intent append, the staged temp bytes,
//! and the committed-record append that follows the atomic replace. The
//! rollback appends stay healthy on purpose: a transient full disk is
//! routinely reclaimed before cleanup runs, and keeping rollbacks alive
//! pins the honest recovery classifications instead of the fence path.

use std::io;
use std::sync::{Arc, Mutex};

/// Which writes a fixture wants the filesystem to refuse.
#[derive(Debug, Clone, Default)]
pub struct StorageFaultPlan {
    /// Refuse the authority-journal intent append inside `prepare`.
    /// Nothing is staged and no record exists afterwards.
    pub refuse_prepare_intent_append: bool,
    /// Let only this many staged bytes land, then fail the stage write
    /// with a storage-full error, leaving a truncated temp behind for
    /// the prepare path to clean up itself.
    pub stage_write_budget_bytes: Option<u64>,
    /// Refuse the committed-record append after the atomic replace has
    /// already landed. The caller receives an applied-but-not-durably-
    /// acknowledged receipt; reopen must classify by hash evidence.
    pub refuse_commit_record_append: bool,
}

/// The shared armed state behind one workspace handle.
pub type SharedFaultPlan = Arc<Mutex<Option<StorageFaultPlan>>>;

impl StorageFaultPlan {
    pub fn shared() -> SharedFaultPlan {
        Arc::new(Mutex::new(None))
    }

    /// A storage-full error carrying the failing site, so fixtures can
    /// tell injected refusals apart from real I/O failures.
    pub(crate) fn storage_full(what: &str) -> io::Error {
        io::Error::new(
            io::ErrorKind::StorageFull,
            format!("injected storage full during {what}"),
        )
    }
}

/// Snapshot of the currently armed plan, if any. Always `None` unless a
/// fixture armed this instance.
pub(crate) fn active_plan(state: &SharedFaultPlan) -> Option<StorageFaultPlan> {
    state.lock().expect("storage fault plan poisoned").clone()
}
