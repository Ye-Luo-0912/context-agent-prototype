"""Bounded real-model runner for the incremental-platform campaign."""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
import threading
import time
import urllib.request
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
CAMPAIGN = REPO / "target" / "runtime-endurance-v1" / "incremental-platform-20260919"
WORK = CAMPAIGN / "workspace"


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def protected() -> dict[str, str]:
    names = ["TASK.md", "tests/test_platform.py", "fixtures/input.jsonl", "fixtures/plan.json", "oracle.py"]
    return {name: digest(WORK / name) for name in names}


def usage_from_sse(raw: bytes):
    latest = None
    for line in raw.splitlines():
        if not line.startswith(b"data: ") or line[6:].strip() == b"[DONE]":
            continue
        try:
            event = json.loads(line[6:])
        except Exception:
            continue
        usage = (event.get("response") or {}).get("usage") or event.get("usage")
        if isinstance(usage, dict) and any(k in usage for k in ("input_tokens", "output_tokens", "cached_input_tokens")):
            latest = usage
    return latest


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("segment")
    parser.add_argument("--mode", choices=["work", "resume", "feedback"], default="work")
    parser.add_argument("--prompt-file")
    parser.add_argument("--rounds", type=int, default=24)
    parser.add_argument("--max-cost-usd", type=float, default=1.5)
    parser.add_argument("--max-output-tokens", type=int, default=8192)
    args = parser.parse_args()
    out = CAMPAIGN / args.segment
    out.mkdir(parents=False)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("OPENAI_", "MAINTENANCE_")) and k not in ("AGENT_AUTO_APPROVE", "AGENT_DEMO")}
    for line in (REPO / "eval.env").read_text(encoding="utf-8-sig").splitlines():
        if "=" in line and not line.lstrip().startswith("#"):
            key, value = line.split("=", 1)
            env[key.strip()] = value.strip().strip('"').strip("'")
    if env.get("OPENAI_MODEL") != "deepseek-flash":
        raise RuntimeError("campaign requires the configured deepseek-flash profile")
    upstream = env["OPENAI_BASE_URL"].rstrip("/")
    wire: list[dict] = []
    usage_rows: list[dict] = []
    lock = threading.Lock()
    spend = {"usd": 0.0, "unknown": False, "cap_stopped": False}

    class Relay(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):  # noqa: N802
            body = self.rfile.read(int(self.headers["Content-Length"]))
            with lock:
                number = len(wire) + 1
                wire.append({"request": number, "path": self.path})
                if spend["cap_stopped"] or spend["unknown"] or spend["usd"] >= args.max_cost_usd:
                    spend["cap_stopped"] = True
                    self.send_response(429)
                    self.send_header("Content-Type", "application/json")
                    self.end_headers()
                    self.wfile.write(b'{"error":"bounded campaign cost cap reached"}')
                    return
            (out / f"request-{number:03}.json").write_bytes(body)
            request = urllib.request.Request(
                upstream + "/" + self.path.removeprefix("/v1/"),
                data=body,
                headers={"Content-Type": "application/json", "Authorization": self.headers.get("Authorization", ""), "Accept": "text/event-stream"},
                method="POST",
            )
            try:
                response = urllib.request.urlopen(request, timeout=150)
            except urllib.error.HTTPError as error:
                response = error
            except Exception:
                self.send_error(502, "upstream transport failed")
                return
            response_bytes = bytearray()
            with response:
                self.send_response(response.status)
                self.send_header("Content-Type", response.headers.get("Content-Type", "text/event-stream"))
                self.end_headers()
                with (out / f"response-{number:03}.sse").open("wb") as captured:
                    while True:
                        chunk = response.read1(16384)
                        if not chunk:
                            break
                        captured.write(chunk)
                        if len(response_bytes) <= 2 * 1024 * 1024:
                            response_bytes.extend(chunk)
                        try:
                            self.wfile.write(chunk)
                            self.wfile.flush()
                        except (BrokenPipeError, ConnectionResetError):
                            break
            usage = usage_from_sse(bytes(response_bytes))
            with lock:
                if usage is None:
                    spend["unknown"] = True
                else:
                    input_tokens = int(usage.get("input_tokens") or 0)
                    cached = int(usage.get("cached_input_tokens") or 0)
                    output_tokens = int(usage.get("output_tokens") or 0)
                    miss = max(input_tokens - cached, 0)
                    estimate = (miss * 0.30 + cached * 0.006 + output_tokens * 1.20) / 1_000_000
                    spend["usd"] += estimate
                    usage_rows.append({"request": number, "input_tokens": input_tokens, "cached_input_tokens": cached, "output_tokens": output_tokens, "estimated_peak_usd": estimate, "cumulative_peak_usd": spend["usd"]})

    server = ThreadingHTTPServer(("127.0.0.1", 0), Relay)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env.update(
        OPENAI_BASE_URL=f"http://127.0.0.1:{server.server_port}/v1",
        OPENAI_API_PROTOCOL="responses",
        OPENAI_RESPONSES_REASONING_EFFORT="none",
        OPENAI_MAX_OUTPUT_TOKENS=str(args.max_output_tokens),
        OPENAI_PROMPT_CACHE_MODE="provider_default",
        OPENAI_RETRY_METRICS_FILE=str(out / "retry.jsonl"),
    )
    py = env.get("AGENT_PYTHON") or sys.executable
    grants = [
        {"id": "app-write", "risk": "WorkspaceWrite", "target": {"workspace_path_prefix": "app"}, "constraint": {"max_content_bytes": 160000}, "expires_at_ms": int((time.time() + 1800) * 1000)},
        {"id": "python-tests", "risk": "ProcessExecution", "target": {"exec_argv_prefix": [py]}, "constraint": {"max_runs": 48}, "expires_at_ms": int((time.time() + 1800) * 1000)},
    ]
    (out / "grants.json").write_text(json.dumps(grants), encoding="utf-8")
    prompt = f"""Read TASK.md and implement the complete Incremental Build and Publication Platform upgrade in the existing v1 workspace. Use the current language and standard library/dependencies only. Run the immutable public tests first with process.run using executable {py} and arguments -m unittest discover -s tests -v. Implement strict CSV/JSONL validation, deterministic DAG operators and independent reference checks, content/config fingerprints, immutable CAS manifests and atomic GC, 4-8 worker processes with leases/stale-token fencing/retries, a durable outbox with idempotent localhost publication and receipt reconciliation, an interrupted v1-to-v2 migration, CLI/API, README, DESIGN, recovery runbook and RESULT.json. Add meaningful app tests for independent process contention and crashes under app/tests; do not modify TASK.md, tests, fixtures or oracle.py. Never execute payloads. Work only in app/ and be honest about incomplete work; ordinary final is not operator acceptance. Preserve all current requirements across later corrections."""
    if args.prompt_file:
        prompt = (CAMPAIGN / args.prompt_file if not Path(args.prompt_file).is_absolute() else Path(args.prompt_file)).read_text(encoding="utf-8").replace("{python}", py)
    (out / "prompt.txt").write_text(prompt, encoding="utf-8")
    dirty = subprocess.check_output(["git", "diff", "--binary"], cwd=REPO)
    (out / "runtime.patch").write_bytes(dirty)
    binary = REPO / "target/debug/agent-tui.exe"
    metadata = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip(), "dirty_diff_sha256": hashlib.sha256(dirty).hexdigest(), "binary_sha256": digest(binary), "protected_before": protected(), "round_cap": args.rounds, "max_output_tokens": args.max_output_tokens, "cost_cap_usd": args.max_cost_usd, "model": env["OPENAI_MODEL"], "mode": args.mode, "wire_capture": "forwarding relay; credentials omitted"}
    (out / "metadata.json").write_text(json.dumps(metadata, indent=2), encoding="utf-8")
    action = ["--work", "--prompt=-"] if args.mode == "work" else (["--restore=latest", "--continue"] if args.mode == "resume" else ["--restore=latest", "--prompt=-"])
    command = [str(binary), *action, "--context=dynamic", f"--max-rounds={args.rounds}", "--timeout-secs=720", f"--grant-file={out / 'grants.json'}", f"--jsonl-out={out / 'events.jsonl'}", str(WORK)]
    started = time.monotonic()
    try:
        with (out / "stderr.log").open("w", encoding="utf-8") as error, (out / "stdout.log").open("w", encoding="utf-8") as stdout:
            process = subprocess.Popen(command, cwd=WORK, env=env, stdin=subprocess.PIPE, stdout=stdout, stderr=error, text=True, encoding="utf-8")
            (out / "pid.txt").write_text(str(process.pid), encoding="utf-8")
            process.communicate(None if args.mode == "resume" else prompt, timeout=780)
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
        (out / "usage-ledger.json").write_text(json.dumps({"cost_cap_usd": args.max_cost_usd, "estimated_peak_usd": spend["usd"], "usage_known": not spend["unknown"], "cap_stopped": spend["cap_stopped"], "rows": usage_rows}, indent=2), encoding="utf-8")
    metadata.update({"exit": process.returncode, "elapsed": round(time.monotonic() - started, 2), "requests": len(wire), "protected_after": protected(), "protected_unchanged": protected() == metadata["protected_before"]})
    (out / "metadata.json").write_text(json.dumps(metadata, indent=2), encoding="utf-8")
    rows = [json.loads(line) for line in (out / "events.jsonl").read_text(encoding="utf-8").splitlines() if line.strip()]
    events = [row["event"] for row in rows if "event" in row]
    summary = {"exit": process.returncode, "elapsed": metadata["elapsed"], "rounds": sum(event.get("type") == "model_started" for event in events), "tool_calls": sum(event.get("type") == "tool_finished" for event in events), "protected_unchanged": metadata["protected_unchanged"], "terminals": [event for event in events if event.get("type") in ("turn_failed", "turn_completed", "recovery_required")], "session": rows[-1], "requests": len(wire)}
    (out / "summary.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False))


if __name__ == "__main__":
    main()
