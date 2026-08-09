//! `Capability` implemented over the shared `ProcessHost`: the generic
//! process-capability adapter. The host lives in `agent-process`; this crate
//! is only the protocol layer translating `Capability` calls onto
//! `{"op": "invoke", "call": ...}` and back, so a process capability never
//! writes its own stdio framing — the host owns that once.

mod capability_host;

pub use capability_host::{ProcessCapabilityAdapter, load_process_capability};
