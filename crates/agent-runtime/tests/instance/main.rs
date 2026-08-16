//! `RuntimeInstance` shutdown tests: the instance owns the host, the handle
//! and the actor task, and `shutdown` runs the ordered teardown while
//! aggregating errors instead of swallowing them.

mod anchor;
mod completion;
mod harness;
mod restore;
mod shutdown;
