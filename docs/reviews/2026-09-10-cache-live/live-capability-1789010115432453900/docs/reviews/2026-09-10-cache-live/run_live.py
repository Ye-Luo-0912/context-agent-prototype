"""Run one bounded, explicitly requested live check; never print credentials."""
import os
import hashlib
import json
import subprocess
import sys
import time
from pathlib import Path

repo = Path(__file__).resolve().parents[3]
source = repo / "eval.env"
allowed = {
    "OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL", "OPENAI_API_PROTOCOL",
    "OPENAI_CONTEXT_WINDOW", "OPENAI_MAX_OUTPUT_TOKENS", "OPENAI_TEMPERATURE",
    "OPENAI_PROMPT_CACHE_MODE",
    "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "http_proxy", "https_proxy",
}
child_env = dict(os.environ)
for line in source.read_text(encoding="utf-8-sig").splitlines():
    line = line.strip()
    if not line or line.startswith("#"):
        continue
    if line.startswith("export "):
        line = line[7:].strip()
    name, sep, value = line.partition("=")
    name, value = name.strip(), value.strip()
    if not sep or name not in allowed or child_env.get(name, "").strip():
        continue
    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
        value = value[1:-1]
    child_env[name] = value

if not child_env.get("OPENAI_API_KEY", "").strip():
    raise SystemExit("Existing provider credential unavailable; no request sent.")
if child_env.get("AGENT_DEMO", "").lower() in {"1", "true"}:
    raise SystemExit("Demo mode cannot provide live cache evidence; no request sent.")

scenarios = {
    "first": ("compare_layouts_with_bounded_live_requests", 10),
    "isolated": ("compare_isolated_warmed_layouts", 12),
    "boundary": ("boundary::probe_explicit_reuse_boundary", 4),
    "capability": ("capability::probe_cache_parameter_capability", 2),
}
scenario = sys.argv[1] if len(sys.argv) == 2 else "first" if len(sys.argv) == 1 else ""
if scenario not in scenarios:
    raise SystemExit("Choose exactly one scenario: first, isolated, boundary or capability. No request sent.")
test_name, max_calls = scenarios[scenario]
if scenario == "boundary":
    max_calls = int(child_env.get("KV_CACHE_MAX_REQUESTS", "4"))
    if not 1 <= max_calls <= 4:
        raise SystemExit("Boundary request cap must be in 1..4. No request sent.")
report = Path(__file__).with_name(f"live-{scenario}-{time.time_ns()}.json")
child_env["OPENAI_MAX_OUTPUT_TOKENS"] = "128" if scenario == "capability" else "512"
child_env["OPENAI_PROMPT_CACHE_MODE"] = "responses_explicit" if scenario == "boundary" else "provider_default"
child_env["KV_CACHE_LIVE"] = "1"
child_env["KV_CACHE_REPORT"] = str(report)
command = ["cargo", "test", "-p", "agent-compose", "--test", "kv_cache_walk",
           test_name, "--", "--ignored", "--exact",
           "--nocapture", "--test-threads=1"]
print(f"Bounded live check: <={max_calls} requests, <={child_env['OPENAI_MAX_OUTPUT_TOKENS']} output tokens per request; report={report.name}", flush=True)
if scenario in {"boundary", "capability"}:
    sources = [
        "Cargo.lock", "crates/agent-contracts/src/model.rs", "crates/agent-contracts/src/model_cache.rs",
        "crates/agent-runtime/src/prompt.rs", "crates/agent-runtime/src/actor/model.rs",
        "crates/agent-compose/src/lib.rs", "crates/agent-compose/tests/kv_cache_walk.rs",
        "crates/agent-compose/tests/kv_cache_walk/boundary.rs", "crates/provider-openai/src/lib.rs",
        "crates/agent-compose/tests/kv_cache_walk/capability.rs",
        "crates/provider-openai/src/prompt_cache.rs", "crates/provider-openai/src/diagnostics.rs",
        "crates/provider-openai/src/responses.rs", "crates/provider-openai/src/wire_names.rs",
        "docs/reviews/2026-09-10-cache-live/run_live.py",
    ]
    snapshot = report.with_suffix("")
    snapshot.mkdir(exist_ok=False)
    source_hashes = {}
    for relative in sources:
        data = (repo / relative).read_bytes()
        source_hashes[relative] = hashlib.sha256(data).hexdigest()
        destination = snapshot / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
    manifest = {"head":subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip(),
                "sources":source_hashes, "unchanged_after_run":False}
    (snapshot / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
result = subprocess.run(command, cwd=repo, env=child_env, check=False)
if scenario in {"boundary", "capability"}:
    manifest["unchanged_after_run"] = all(
        hashlib.sha256((repo / relative).read_bytes()).hexdigest() == digest
        for relative, digest in source_hashes.items())
    (snapshot / "manifest.json").write_text(json.dumps(manifest, indent=2), encoding="utf-8")
    if not manifest["unchanged_after_run"]:
        raise SystemExit("Source changed during the probe; report is not source-bound.")
print(f"Report path: {report}", flush=True)
raise SystemExit(result.returncode)
