//! The real product binary's startup preflight, end to end. A bad model
//! configuration must fail as a visible configuration error BEFORE any
//! workspace state exists (M16-01 preflight), and the same binary must
//! complete a real headless run when the demo transport is selected
//! (M16-07: the actual binary, not an in-process shortcut).

use std::process::Command;

#[test]
fn a_missing_model_config_fails_before_creating_workspace_state() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-tui"));
    command
        .arg(&root)
        .arg("--prompt=hello")
        .env_remove("AGENT_DEMO")
        .env_remove("OPENAI_API_KEY");
    let output = command.output().unwrap();

    assert!(
        !output.status.success(),
        "a missing model config must fail the run"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no model configured"),
        "the error must name the fix: {stderr}"
    );
    assert!(
        !root.join(".focus-agent").exists(),
        "a failed model preflight must leave no workspace state behind"
    );
}

#[test]
fn the_real_binary_completes_a_headless_demo_run() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let jsonl = temp.path().join("out.jsonl");

    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-tui"));
    command
        .arg(root)
        .arg("--prompt=demo: say hello")
        .arg(format!("--jsonl-out={}", jsonl.display()))
        .env("AGENT_DEMO", "1")
        .env_remove("OPENAI_API_KEY");
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "the headless demo run must succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let jsonl_text = std::fs::read_to_string(&jsonl).unwrap();
    assert!(
        jsonl_text.contains("\"kind\":\"session_end\""),
        "the JSONL must carry the session_end row: {jsonl_text}"
    );
    assert!(
        jsonl_text.contains("awaiting_operator_review"),
        "the real binary must carry the closure semantics: {jsonl_text}"
    );
}
