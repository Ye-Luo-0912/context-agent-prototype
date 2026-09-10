"""Run one bounded, explicitly requested live check; never print credentials."""
import os
import subprocess
import sys
import time
from pathlib import Path

repo = Path(__file__).resolve().parents[3]
source = repo / "eval.env"
allowed = {
    "OPENAI_API_KEY", "OPENAI_BASE_URL", "OPENAI_MODEL", "OPENAI_API_PROTOCOL",
    "OPENAI_CONTEXT_WINDOW", "OPENAI_MAX_OUTPUT_TOKENS", "OPENAI_TEMPERATURE",
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
}
scenario = sys.argv[1] if len(sys.argv) == 2 else "first" if len(sys.argv) == 1 else ""
if scenario not in scenarios:
    raise SystemExit("Choose exactly one scenario: first or isolated. No request sent.")
test_name, max_calls = scenarios[scenario]
report = Path(__file__).with_name(f"live-{scenario}-{time.time_ns()}.json")
child_env["OPENAI_MAX_OUTPUT_TOKENS"] = "512"
child_env["KV_CACHE_LIVE"] = "1"
child_env["KV_CACHE_REPORT"] = str(report)
command = ["cargo", "test", "-p", "agent-compose", "--test", "kv_cache_walk",
           test_name, "--", "--ignored", "--exact",
           "--nocapture", "--test-threads=1"]
print(f"Bounded live check: <={max_calls} requests, <=512 output tokens per request; report={report.name}", flush=True)
result = subprocess.run(command, cwd=repo, env=child_env, check=False)
print(f"Report path: {report}", flush=True)
raise SystemExit(result.returncode)
