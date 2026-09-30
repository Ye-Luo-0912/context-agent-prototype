//! Turn execution and streaming behavior at the actor level: the five-layer
//! model input, the execution stack (Turn Frame) versus the long-term
//! working set (Context Frame), and cancellation of a hanging model round.

mod adaptive_history;
mod authority_exhaustion;
mod combined_cancel;
mod completion;
mod directive;
mod effects;
mod failure_resume;
mod focus;
mod harness;
mod incremental_cancel;
mod ingest_cancel;
mod input_bounds;
mod maintenance;
mod opportunity;
mod policy;
mod prompt_layout;
mod recovery_surface;
mod safepoint;
mod scopes;
mod settlement;
mod stream;
mod task_progress;
