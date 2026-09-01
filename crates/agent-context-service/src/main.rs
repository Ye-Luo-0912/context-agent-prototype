//! Standalone context service.
//!
//! Reads JSON requests from stdin and writes JSON responses to stdout (one
//! line each, see `context-contextcore::wire`). It runs a real in-process
//! `ContextEngine` chosen with `--engine`; the adapter crate is the client.
//! A future real ContextCore runtime only has to speak the same protocol —
//! nothing on the agent side changes. The protocol handling lives in
//! `agent_context_service::handle`; this binary is only the stdio loop.
//!
//! The session uses the same bounded frame codec as the `ProcessHost` client,
//! symmetric in both directions. The implementation lives in the library so
//! every failure mode can be tested without relying on OS pipe packetization.

use agent_context_service::{build_engine, serve_session};
use agent_contracts::ContextEngine;
use context_contextcore::{
    DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES, MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES,
};
use tokio::io::{BufReader, BufWriter};

fn usage() -> ! {
    eprintln!(
        "usage: agent-context-service --engine <dynamic|append|rolling> [--store-dir <path>]\n\
         \x20      [--max-frame-bytes <bytes>]\n\
         \n\
         Speaks the context-contextcore wire protocol on stdin/stdout.\n"
    );
    std::process::exit(2);
}

#[tokio::main]
async fn main() {
    let mut engine = None;
    let mut store_dir = None;
    let mut max_frame_bytes = DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--engine" => {
                let Some(value) = args.next() else {
                    usage();
                };
                engine = Some(value);
            }
            "--store-dir" => {
                let Some(value) = args.next() else {
                    usage();
                };
                store_dir = Some(std::path::PathBuf::from(value));
            }
            "--max-frame-bytes" => {
                let Some(value) = args.next() else {
                    usage();
                };
                max_frame_bytes = value.parse().expect("--max-frame-bytes must be a number");
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    let engine_name = engine.expect("--engine is required");
    if !(MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES..=DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES)
        .contains(&max_frame_bytes)
    {
        eprintln!(
            "--max-frame-bytes must be in {MIN_CONTEXT_SERVICE_MAX_FRAME_BYTES}..={DEFAULT_CONTEXT_SERVICE_MAX_FRAME_BYTES}"
        );
        std::process::exit(2);
    }
    let engine = build_engine(&engine_name, store_dir.clone());
    let engine: &dyn ContextEngine = engine.as_ref();

    // Startup store reconcile, part of the serve transaction: converge the
    // on-disk blob directory with the external map before serving framed
    // operations (crash-recovery authority over formal blobs). It runs only
    // when the service owns an explicit store — the temp-dir fallback is
    // not a persistence domain to reconcile. Refuse to serve on failure
    // instead of running a session over an unreconciled store.
    if store_dir.is_some()
        && let Err(problem) = engine.reconcile_store().await
    {
        eprintln!("startup store reconcile failed: {problem}");
        std::process::exit(1);
    }

    let mut stdin = BufReader::new(tokio::io::stdin());
    let stdout = tokio::io::stdout();
    let mut writer = BufWriter::new(stdout);

    match serve_session(&mut stdin, &mut writer, engine, max_frame_bytes).await {
        // Clean EOF or a graceful Shutdown request: exit 0.
        Ok(agent_context_service::SessionEnd::Clean) => {}
        // A terminal protocol violation already received an error frame;
        // exit non-zero so a supervisor never mistakes the dead protocol
        // run for a healthy service.
        Ok(agent_context_service::SessionEnd::ProtocolViolation) => {
            eprintln!("context service: terminal protocol violation, exiting non-zero");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("context service session failed: {error}");
            std::process::exit(1);
        }
    }
}
