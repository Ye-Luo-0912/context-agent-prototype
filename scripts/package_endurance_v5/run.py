"""Start one continuous V5 model task alongside the load controller.

Two review items are closed here:

* F10 - the ledger's declared reservation strategy and the executed reservation
  come from one place (``BudgetLedger.estimate_reserve_usd`` and its parameter
  attributes), and the cross-segment counters are derived from the ledger and
  the segment summaries instead of being trusted from the caller;
* F05 - the campaign owns the write/load lifetime: the frozen load is not killed
  just because a model segment stopped for any reason, and the pure-local load
  keeps running to the frozen bound.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import campaign_accounting  # noqa: E402
import runner_grants  # noqa: E402
from campaign_accounting import campaign_accounting as campaign_accounting_report  # noqa: E402
from campaign_accounting import limits, remaining_allowance  # noqa: E402
from runtime_endurance_incremental_runner import BudgetLedger, RunnerConfig, build_env, run_segment  # noqa: E402


class V5Ledger(BudgetLedger):
    """Wire-byte reservation: the declared policy text is generated from the
    same two numbers the amount is computed from, so they cannot drift."""

    reserve_strategy = "wire_bytes_plus_8192_and_full_output"
    reserve_input_tokens_per_byte = 1.0
    reserve_input_padding_tokens = 8192

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


def segment_summaries(stage):
    return campaign_accounting.segment_summaries(stage)


def campaign_accounting_report_for(stage):
    return campaign_accounting_report(stage)


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
            "and repair concrete failures. Feedback lists unresolved failures; a new batch number is not "
            "progress. Do not declare success from a smoke test or from text; the controller must report a "
            "passing continuous load and the final public tests must pass. Ordinary final is not operator "
            "acceptance.\n\nStart by reading SPEC.md, the public app interfaces and runtime-feedback/latest.json, "
            "then establish the current failure before implementing.")


def next_load_name(stage, resume):
    """The controller output name of this window; never reuses an existing one."""
    stage = Path(stage)
    if not resume:
        return "continuous-load"
    index = 1
    while (stage / f"continuous-load-resume-{index:02d}").exists():
        index += 1
    return f"continuous-load-resume-{index:02d}"


def latest_repository(stage):
    """The newest load repository, so a resume continues the same authority."""
    stage = Path(stage)
    candidates = [path / "repository" for path in sorted(stage.glob("continuous-load*"))
                  if (path / "repository").is_dir()]
    return candidates[-1] if candidates else None


def run(stage, *, seconds=None, rounds=None, resume=False, segment=None, load_wait_seconds=None,
        finalize_load=True):
    stage = Path(stage).resolve()
    caps = limits(stage)
    seconds = caps["target_load_seconds"] if seconds is None else seconds
    allowance = remaining_allowance(stage)
    requested = caps["main_decisions"] if rounds is None else rounds
    rounds = min(requested, allowance["main_decisions"])
    if rounds < 1:
        raise ValueError("the campaign decision budget is exhausted: "
                         f"{allowance['accounting']['main_decisions']} of "
                         f"{caps['main_decisions']} decisions already used")
    if not isinstance(seconds, int) or seconds < 1 or seconds > caps["target_load_seconds"]:
        raise ValueError("load seconds must be within the original V5 bound")
    grants = runner_grants.campaign_grants(caps, python=sys.executable)
    compatibility = runner_grants.compatibility(caps, grants)
    if not compatibility["compatible"]:
        raise ValueError("authorization is not compatible with the frozen task: "
                         + "; ".join(compatibility["problems"]))
    segment = segment or ("model-continuous-repair-01" if resume else "model-continuous")
    load_name = next_load_name(stage, resume)
    load_out = stage / load_name
    if load_out.exists():
        raise FileExistsError("continuous load output already exists")
    load_command = [sys.executable, "-B", str(Path(__file__).with_name("continuous_load.py")),
                    str(stage), "--seconds", str(seconds), "--seed", "20260921",
                    "--out-name", load_name]
    if resume:
        previous_root = latest_repository(stage)
        if previous_root is None:
            raise FileNotFoundError("cannot resume without the original load repository")
        load_command.extend(["--root", str(previous_root), "--resume"])
    load_log = (stage / "continuous-load-controller.log").open("w", encoding="utf-8")
    loader_env = {key: value for key, value in os.environ.items()
                  if not key.startswith("OPENAI_") and key not in ("AGENT_AUTO_APPROVE", "AGENT_DEMO")}
    loader_env["PYTHONDONTWRITEBYTECODE"] = "1"
    loader = subprocess.Popen(load_command, cwd=ROOT, stdout=load_log, stderr=subprocess.STDOUT,
                              env=loader_env, text=True)
    (stage / "continuous-load.pid").write_text(str(loader.pid), encoding="utf-8")
    wait_bound = caps["target_load_seconds"] if load_wait_seconds is None else load_wait_seconds
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
            prompt += ("\n\nThe previous segment stopped without an accepted result. Continue the same task in "
                       "the same repository: read runtime-feedback/latest.json, repair the unresolved failures "
                       "in order, and keep changes bounded. New keys and the generation read from the "
                       "repository authority are used for new requests; do not replay old identities.")
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
            grants=grants,
            # The campaign tool budget is enforced in flight: the baseline is
            # what earlier segments already spent (derived from the campaign
            # materials), so a resume cannot buy a fresh tool allowance.
            tool_budget=caps["tool_attempts"],
            tool_budget_baseline=allowance["accounting"]["tool_attempts"],
            child_wait_timeout_s=min(caps["deadline_epoch"] - time.time() - 60, 6 * 3600),
        )
        result = run_segment(config)
        segment_end = time.time()
        # The campaign owns the load lifetime: a stopped model segment does not
        # stop the pure-local frozen load.
        deadline = min(caps["deadline_epoch"], segment_end + max(wait_bound, 0))
        while loader.poll() is None and time.time() < deadline:
            time.sleep(1.0)
        receipt = dict(segment_exit=result, segment_end_epoch=round(segment_end, 3),
                       load_running_after_segment=loader.poll() is None,
                       load_wait_seconds=max(wait_bound, 0), allowance=allowance,
                       authorization=compatibility, grants=grants,
                       tool_budget=dict(cap=caps["tool_attempts"],
                                        baseline=allowance["accounting"]["tool_attempts"]),
                       accounting=campaign_accounting_report(stage))
        (stage / "segment-handoff.json").write_text(
            json.dumps(receipt, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        return result
    finally:
        terminated = False
        if loader.poll() is None:
            terminated = True
            loader.terminate()
            try:
                loader.wait(timeout=20)
            except subprocess.TimeoutExpired:
                loader.kill()
                loader.wait(timeout=10)
        load_log.close()
        (stage / "continuous-load-exit.json").write_text(
            json.dumps(dict(returncode=loader.returncode, terminated_by_campaign=terminated,
                            finalize_load=finalize_load), indent=2) + "\n", encoding="utf-8")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("--seconds", type=int)
    parser.add_argument("--rounds", type=int)
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--segment")
    parser.add_argument("--load-wait-seconds", type=int)
    args = parser.parse_args()
    raise SystemExit(run(args.stage, seconds=args.seconds, rounds=args.rounds,
                         resume=args.resume, segment=args.segment,
                         load_wait_seconds=args.load_wait_seconds))
