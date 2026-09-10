"""Bounded Flash-only live check. Credential is process-local, never persisted."""
import hashlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path


def main():
    repo = Path(__file__).resolve().parents[3]
    key = os.environ.get("DEEPSEEK_API_KEY")
    if not key:
        key = sys.stdin.readline(4097).strip()
    if not key or len(key) > 4096:
        raise SystemExit("A process-local DeepSeek credential is required; nothing sent.")
    env = dict(os.environ)
    for name in ["DEEPSEEK_API_KEY", "OPENAI_TEMPERATURE", "AGENT_DEMO", "OPENAI_RETRY_METRICS_FILE"]:
        env.pop(name, None)
    env.update({"OPENAI_API_KEY":key,"OPENAI_BASE_URL":"https://api.deepseek.com",
                "OPENAI_MODEL":"deepseek-flash","OPENAI_API_PROTOCOL":"responses",
                "OPENAI_PROMPT_CACHE_MODE":"provider_default", "OPENAI_RESPONSES_REASONING_EFFORT":"none",
                "OPENAI_CONTEXT_WINDOW":"128000","OPENAI_MAX_OUTPUT_TOKENS":"512","KV_CACHE_LIVE":"1"})
    report = Path(__file__).with_name(f"live-deepseek-{time.time_ns()}.json")
    env["KV_CACHE_REPORT"] = str(report)
    snapshot = report.with_suffix("")
    snapshot.mkdir(exist_ok=False)
    sources = ["Cargo.lock","crates/agent-contracts/src/model.rs","crates/agent-contracts/src/model_cache.rs",
               "crates/agent-runtime/src/prompt.rs","crates/agent-runtime/src/actor/model.rs",
               "crates/agent-compose/src/lib.rs","crates/agent-compose/tests/kv_cache_walk.rs",
               "crates/agent-compose/tests/kv_cache_walk/automatic.rs",
               "crates/provider-openai/src/lib.rs","crates/provider-openai/src/reasoning.rs",
               "crates/provider-openai/src/prompt_cache.rs","crates/provider-openai/src/diagnostics.rs",
               "crates/provider-openai/src/responses.rs","crates/provider-openai/src/sse.rs",
               "crates/provider-openai/src/wire_names.rs","docs/reviews/2026-09-10-cache-live/run_deepseek.py"]
    hashes = {}
    for relative in sources:
        data = (repo / relative).read_bytes()
        if key.encode() in data:
            raise SystemExit("Credential detected in source; no request sent.")
        hashes[relative] = hashlib.sha256(data).hexdigest()
        destination = snapshot / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
    manifest = {"head":subprocess.check_output(["git","rev-parse","HEAD"],cwd=repo,text=True).strip(),
                "sources":hashes,"unchanged_after_run":False}
    manifest_path = snapshot / "manifest.json"
    manifest_path.write_text(json.dumps(manifest,indent=2),encoding="utf-8")
    print(f"DeepSeek Flash: <=10 calls, <=512 output tokens each, no retries; report={report.name}",flush=True)
    command = ["cargo","test","-p","agent-compose","--test","kv_cache_walk",
               "automatic::compare_deepseek_flash_automatic_cache","--","--ignored","--exact","--nocapture","--test-threads=1"]
    result = subprocess.run(command,cwd=repo,env=env,check=False)
    manifest["unchanged_after_run"] = all(hashlib.sha256((repo / path).read_bytes()).hexdigest()==digest for path,digest in hashes.items())
    manifest_path.write_text(json.dumps(manifest,indent=2),encoding="utf-8")
    if report.exists() and key.encode() in report.read_bytes():
        raise SystemExit("Credential detected in report; do not share the artifact.")
    print(f"Report path: {report}",flush=True)
    if not manifest["unchanged_after_run"]:
        raise SystemExit("Source changed during the run; evidence does not bind the current tree.")
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
