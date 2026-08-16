# Context Bench freeze — local results 2026-08-17

Windows (`rustup run stable-x86_64-pc-windows-msvc`):

- `cargo test -p agent-eval`: 84 passed
- `cargo test -p agent-capability-process`: 17 lib + 24 integration passed, including `mcp_cancel_after_spawn_terminates_the_server_tree` (READY barrier, no extra timeout)
- `cargo clippy -p agent-eval -p agent-capability-process --all-targets -- -D warnings`: ok
- `cargo run -p agent-eval -- --context-bench`: 12/12 ok

Frozen identity:

- schema `agent-eval.context-bench.v1`
- `spec_sha256=12dc8e22f3a649b619f719f4a18e0cf73486a668aded4912ca93a469b22bc902`
- `pack_digest=00a6079ee601cd0004060acb168603c80d5d77dc62e77caf1782eccd88e2d38e`

Linux CI is the second platform. Wave-1 live (27 cells) is not in this commit.
