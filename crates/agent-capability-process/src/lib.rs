//! `Capability` implemented over the shared `ProcessHost`: the generic
//! process-capability adapter. The host lives in `agent-process`; this crate
//! is only the protocol layer translating `Capability` calls onto
//! `{"op": "invoke", "call": ...}` and back, so a process capability never
//! writes its own stdio framing — the host owns that once.
//!
//! The crate also carries the MCP adapter: an MCP server over stdio
//! (JSON-RPC 2.0) as a `Capability`, behind the same capability/effect/
//! output boundary.

mod capability_host;
mod mcp;

pub use capability_host::{ProcessCapabilityAdapter, load_process_capability};
pub use mcp::{
    DEFAULT_MCP_MAX_FRAME_BYTES, DEFAULT_MCP_REQUEST_TIMEOUT, MAX_MCP_TOOL_TEXT_CHARS,
    MCP_PROTOCOL_VERSION, McpCallResult, McpCapabilityAdapter, McpClient, McpServerDecl,
    McpStdioClient, McpTool,
};
