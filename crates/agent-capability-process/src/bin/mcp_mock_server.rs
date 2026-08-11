//! A minimal MCP server over stdio for integration tests: speaks JSON-RPC
//! 2.0 (one document per line) and answers `initialize`, `tools/list` and
//! `tools/call`. Built as a sibling of the test binaries
//! (`target/<profile>/mcp_mock_server[.exe]`) so `McpCapabilityAdapter`
//! tests can spawn it like a real MCP server.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request.get("id");
        if id.is_none() {
            continue; // notification: no response
        }
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "mcp-mock-server", "version": "0.1.0"}
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"tools": [
                    {"name": "mock.echo", "description": "echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
                    {"name": "mock.add", "description": "add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}},
                    {"name": "mock.fail", "description": "always report an error", "inputSchema": {"type": "object"}}
                ]}
            }),
            "tools/call" => {
                let name = request["params"]["name"].as_str().unwrap_or("");
                let arguments = &request["params"]["arguments"];
                match name {
                    "mock.echo" => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": arguments["text"].as_str().unwrap_or("")}]}
                    }),
                    "mock.add" => {
                        let a = arguments["a"].as_f64().unwrap_or(0.0);
                        let b = arguments["b"].as_f64().unwrap_or(0.0);
                        json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text", "text": format!("{}", a + b)}]}
                        })
                    }
                    "mock.fail" => json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": "deliberate failure"}], "isError": true}
                    }),
                    _ => json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": "method not found"}
                    }),
                }
            }
            _ => json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "method not found"}
            }),
        };
        let Ok(mut frame) = serde_json::to_string(&response) else {
            break;
        };
        frame.push('\n');
        if out.write_all(frame.as_bytes()).is_err() {
            break;
        }
        if out.flush().is_err() {
            break;
        }
    }
}
