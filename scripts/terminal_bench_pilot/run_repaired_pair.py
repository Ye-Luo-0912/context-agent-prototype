"""Explicit bounded TB8 pair; credentials never enter Harbor or its containers."""
from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

from terminal_bench_pilot.credential_relay import CredentialRelay
from terminal_bench_pilot.outcome import summarize_trial
from runtime_endurance_incremental_runner import build_env


def main():
    repo = Path(__file__).resolve().parents[2]
    cache = Path.home() / ".cache/context-agent-terminal-bench"
    binary = repo / "target-debian12/x86_64-unknown-linux-gnu/debug/agent-tui"
    template = json.loads((cache / "tb7-r-live-job.json").read_text())
    job_root = cache / "jobs"
    report_dir = cache / (sys.argv[1] if len(sys.argv) > 1 else "tb8-repaired-pair-20260922")
    report_dir.mkdir(exist_ok=False)
    credentials = build_env(repo)
    if credentials.get("OPENAI_API_PROTOCOL") != "chat":
        raise ValueError("TB8 requires the configured Chat protocol")
    gateway = json.loads(subprocess.check_output([
        "docker", "network", "inspect", "bridge"], text=True))[0]["IPAM"]["Config"][0]["Gateway"]
    identity = {"protocol": "TB8_REPAIRED_PAIR_20260922", "arms": ["rolling", "dynamic"],
                "rounds_per_arm": 40, "requests_per_arm": 40,
                "input_bytes_per_request": 262144, "output_tokens_per_request": 8192,
                "agent_timeout_secs": 1200, "model": "deepseek-flash",
                "maintenance_calls": 0, "maintenance_tokens": 0, "retries": 0,
                "hashes": {str(path.relative_to(repo)): hashlib.sha256(path.read_bytes()).hexdigest()
                           for path in [binary, Path(__file__),
                               Path(__file__).with_name("harbor_agent.py"),
                               Path(__file__).with_name("credential_relay.py"),
                               Path(__file__).with_name("read_probe.py")]},
                "grant_sha256": hashlib.sha256((cache / "grants-live.json").read_bytes()).hexdigest()}
    (report_dir / "identity.json").write_text(json.dumps(identity, indent=2))
    all_results = []
    for arm in identity["arms"]:
        config = json.loads(json.dumps(template))
        name = f"tb8-{arm}-repaired-40-20260922"
        config["job_name"] = name
        config["agents"][0]["kwargs"]["context"] = arm
        config["agents"][0]["extra_allowed_hosts"] = [gateway]
        config["agents"][0]["env"] = {
            "OPENAI_CHAT_THINKING": "disabled", "OPENAI_MAX_OUTPUT_TOKENS": "8192",
            "OPENAI_CONTEXT_WINDOW": "32768"}
        config_path = report_dir / f"{arm}-config.json"
        config_path.write_text(json.dumps(config, indent=2))
        relay = CredentialRelay(upstream=credentials["OPENAI_BASE_URL"],
                                credential=credentials["OPENAI_API_KEY"],
                                model="deepseek-flash", bind=gateway)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(("OPENAI_", "DEEPSEEK_", "ANTHROPIC_", "AZURE_OPENAI_",
                                      "GOOGLE_", "GEMINI_", "TB_RELAY_", "MAINTENANCE_"))}
        env.update(OPENAI_MODEL="deepseek-flash", OPENAI_API_PROTOCOL="chat",
                   TB_RELAY_BASE_URL=relay.base_url, TB_RELAY_TOKEN=relay.token,
                   PYTHONPATH=str(repo / "scripts"))
        try:
            # Supplier-free reachability check from a disposable task-image
            # container, not merely --help or host loopback. No API call.
            probe = "import urllib.request,urllib.error;\ntry:\n urllib.request.urlopen(urllib.request.Request('" + relay.base_url + "/probe', data=b'x'), timeout=10)\nexcept urllib.error.HTTPError as e:\n assert e.code == 404\n"
            smoke_name = "context-agent-tb8-relay-smoke"
            try:
                subprocess.run(["docker", "run", "--rm", "--name", smoke_name,
                                "--network", "bridge", "--entrypoint", "python3",
                                "harborframework/terminal-bench:live-database-cutover-environment-5997c5b486ffe2b0",
                                "-c", probe], env=env, check=True, timeout=30,
                               stdout=subprocess.DEVNULL)
            finally:
                subprocess.run(["docker", "rm", "-f", smoke_name], env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20)
            print(f"{arm}: container-to-relay smoke passed; starting bounded trial", flush=True)
            with (report_dir / f"{arm}-harbor.log").open("w") as log:
                completed = subprocess.run([str(Path(sys.executable).with_name("harbor")), "run",
                                            "--config", str(config_path)], cwd=repo, env=env,
                                           stdout=log, stderr=subprocess.STDOUT, timeout=2400)
            trials = list((job_root / name).glob("live-database-cutover__*/result.json"))
            result = {"arm": arm, "harbor_exit": completed.returncode,
                      "trials": [summarize_trial(path.parent) for path in trials]}
            all_results.append(result)
            print(json.dumps(result), flush=True)
        finally:
            relay.close()
            relay.save_receipt(report_dir / f"{arm}-relay.json")
            (report_dir / "results.json").write_text(json.dumps(all_results, indent=2))
    print("pair finished; evidence: " + str(report_dir), flush=True)


if __name__ == "__main__":
    main()
