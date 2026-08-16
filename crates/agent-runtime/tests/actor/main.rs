//! Actor tests: command serialization, busy rejection, cancellation and
//! stale-result dropping. Uses minimal stubs for context/tools/model so the
//! actor is exercised against the engine contracts only.

mod barrier;
mod budget;
mod busy;
mod focus;
mod harness;
mod input;
mod restore;
mod surface;
