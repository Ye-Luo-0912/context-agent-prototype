//! Turn execution and streaming behavior at the actor level: the five-layer
//! model input, the execution stack (Turn Frame) versus the long-term
//! working set (Context Frame), and cancellation of a hanging model round.

mod completion;
mod effects;
mod focus;
mod harness;
mod policy;
mod scopes;
mod stream;
mod task_progress;
