// Cargo owns the `agent-context-service` binary for integration tests in this
// package and therefore provides `CARGO_BIN_EXE_agent-context-service` for the
// exact freshly-built executable. The contract suite remains beside the
// adapter source it exercises, but its test target belongs to this package.
#[path = "../../context-contextcore/tests/service.rs"]
mod service;
