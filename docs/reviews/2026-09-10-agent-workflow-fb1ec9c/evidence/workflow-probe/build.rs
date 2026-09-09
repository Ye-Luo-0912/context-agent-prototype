use std::{env, fs, path::PathBuf};
fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../../../../");
    let path = root.join("crates/agent-runtime/src/actor/model.rs");
    println!("cargo:rerun-if-changed={}", path.display());
    let source = fs::read_to_string(path).unwrap();
    let start = source.find("fn is_required_context_body(").unwrap();
    let end = source.find("fn settlement_progress_views(").unwrap();
    let helpers = source[start..end]
        .replace(
            "fn record_final_pack_drop(",
            "pub fn record_final_pack_drop(",
        )
        .replace(
            "fn largest_final_pack_drop_index(",
            "pub fn largest_final_pack_drop_index(",
        );
    fs::write(
        PathBuf::from(env::var("OUT_DIR").unwrap()).join("final_pack.rs"),
        format!("use agent_contracts::*;\nuse agent_contracts::tokens::approx_tokens;\n{helpers}"),
    )
    .unwrap();
}
