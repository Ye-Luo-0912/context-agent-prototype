"""Start one continuous V5 model task alongside the load controller."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
from runtime_endurance_incremental_runner import BudgetLedger, RunnerConfig, build_env, run_segment


def limits(stage):
    value = json.loads((Path(stage) / "campaign.json").read_bytes())
    if value.get("kind") != "v5_online_backup_continuous":
        raise ValueError("wrong V5 campaign kind")
    if value["deadline_epoch"] != value["created_epoch"] + 6 * 3600:
        raise ValueError("campaign deadline was changed")
    return value


class V5Ledger(BudgetLedger):
    def _reserve_policy(self):
        return dict(strategy="wire_bytes_plus_8192_and_full_output",
                    max_output_tokens=self.max_output_tokens, **limits(self.path.parent))

    def reserve(self, number, body_len, segment):
        with self.lock:
            caps = limits(self.path.parent)
            admitted = [row for row in self.data["attempts"] if row["status"] != "rejected_cap"]
            reason = None
            if any(row["status"] in ("unknown", "reserved") for row in admitted):
                reason = "unsettled prior attempt"
            elif time.time() >= caps["deadline_epoch"]:
                reason = "campaign deadline"
            elif len(admitted) >= caps["provider_attempts"]:
                reason = "provider attempt cap"
            elif sum(row.get("input_tokens", 0) for row in admitted) + body_len + 8192 > caps["input_tokens"]:
                reason = "input token cap"
            elif sum(row.get("output_tokens", 0) for row in admitted) + self.max_output_tokens > caps["output_tokens"]:
                reason = "output token cap"
            if reason:
                self.data["cap_stopped"] = True
            result = super().reserve(number, body_len, segment)
            if reason:
                self.data["attempts"][-1]["detail"] = reason
                self._save()
            return result


def prompt_text(python):
    return (f"Implement the online backup and safe continuation task in SPEC.md in this existing workspace. "
            f"The controller is already running the same repository and writes runtime-feedback/latest.json; "
            f"read that report with fs.read between repairs. For process.run use argv[0]={python}. "
            "Only app/live_backup.py and app/tests/test_live_backup.py may change. Existing app code, "
            "snapshot.py, SPEC.md, fixtures, public tests, runtime-feedback, controller, oracle and Runtime "
            "private files are protected. Do not access network, execute package payloads, or inspect the "
            "controller/oracle source. Preserve existing install, GC, recovery and publication behavior. "
            "Run the public tests first, then implement backup_live/restore_live with deterministic archives, "
            "complete historical authority, outbox preservation, atomic restore, and crash retry behavior. "
            "Keep this one task alive while the load runs: read fresh feedback, make bounded edits, run tests, "
            "and repair concrete failures. Do not declare success from a smoke test or from text; the controller "
            "must report a passing continuous load and the final public tests must pass. Ordinary final is not "
            "operator acceptance.\n\nStart by reading SPEC.md, the public app interfaces and runtime-feedback/latest.json, "
            "then establish the current failure before implementing.")


def run(stage, *, seconds=None, rounds=None, resume=False, segment=None):
    stage = Path(stage).resolve()
    caps = limits(stage)
    seconds = caps["target_load_seconds"] if seconds is None else seconds
    rounds = caps["main_decisions"] if rounds is None else rounds
    if not isinstance(seconds, int) or seconds < 1 or seconds > caps["target_load_seconds"]:
        raise ValueError("load seconds must be within the original V5 bound")
    if not isinstance(rounds, int) or rounds < 1 or rounds > caps["main_decisions"]:
        raise ValueError("decision rounds must be within the original V5 bound")
    segment = segment or ("model-continuous-repair-01" if resume else "model-continuous")
    load_name = "continuous-load-resume-02" if resume else "continuous-load"
    load_out = stage / load_name
    if load_out.exists():
        raise FileExistsError("continuous load output already exists")
    load_command = [sys.executable, "-B", str(Path(__file__).with_name("continuous_load.py")),
                    str(stage), "--seconds", str(seconds), "--seed", "20260921",
                    "--out-name", load_name]
    if resume:
        previous_root = stage / "continuous-load" / "repository"
        if not previous_root.is_dir():
            raise FileNotFoundError("cannot resume without the original load repository")
        load_command.extend(["--root", str(previous_root)])
    load_log = (stage / "continuous-load-controller.log").open("w", encoding="utf-8")
    loader_env = {key: value for key, value in os.environ.items()
                  if not key.startswith("OPENAI_") and key not in ("AGENT_AUTO_APPROVE", "AGENT_DEMO")}
    loader_env["PYTHONDONTWRITEBYTECODE"] = "1"
    loader = subprocess.Popen(load_command, cwd=ROOT, stdout=load_log, stderr=subprocess.STDOUT,
                              env=loader_env, text=True)
    (stage / "continuous-load.pid").write_text(str(loader.pid), encoding="utf-8")
    try:
        time.sleep(1.0)
        env = build_env(ROOT)
        env["AGENT_PYTHON"] = sys.executable
        env["MAINTENANCE_MAX_CALLS_PER_MAINTAIN"] = "0"
        env["OPENAI_API_PROTOCOL"] = "chat"
        env["OPENAI_CHAT_THINKING"] = "disabled"
        env.pop("OPENAI_RESPONSES_REASONING_EFFORT", None)
        baseline = json.loads((stage / "baseline-lock.json").read_bytes())
        prompt = prompt_text(sys.executable)
        if resume:
            prompt += ("\n\nThe previous turn stopped at model output limit before creating the allowed module. "
                       "Do not spend this turn re-reading the entire repository or changing runner files. "
                       "Immediately create app/live_backup.py with the smallest coherent implementation, "
                       "then run one focused test and inspect the latest feedback. Continue with bounded edits.")
        if len(prompt) > 2000:
            raise ValueError(f"durable task prompt exceeds 2000-character limit: {len(prompt)}")
        config = RunnerConfig(
            segment=segment,
            campaign_dir=stage,
            mode="feedback" if resume else "work",
            rounds=rounds,
            prompt_text=prompt,
            env=env,
            api_protocol="chat",
            protected_files=tuple(baseline["files"]),
            max_cost_usd=caps["estimated_cost_usd"],
            max_output_tokens=8192,
            ledger_class=V5Ledger,
            child_wait_timeout_s=min(caps["deadline_epoch"] - time.time() - 60, 6 * 3600),
        )
        result = run_segment(config)
        return result
    finally:
        if loader.poll() is None:
            loader.terminate()
            try:
                loader.wait(timeout=20)
            except subprocess.TimeoutExpired:
                loader.kill()
                loader.wait(timeout=10)
        load_log.close()
        (stage / "continuous-load-exit.json").write_text(
            json.dumps(dict(returncode=loader.returncode), indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("--seconds", type=int)
    parser.add_argument("--rounds", type=int)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--segment")
    args = parser.parse_args()
    raise SystemExit(run(args.stage, seconds=args.seconds, rounds=args.rounds,
                         resume=args.resume, segment=args.segment))
