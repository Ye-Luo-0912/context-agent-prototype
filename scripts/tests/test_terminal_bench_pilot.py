from contextlib import redirect_stderr
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

from terminal_bench_pilot import harbor_agent, preflight  # noqa: E402
from terminal_bench_pilot.launch_session_credential import (  # noqa: E402
    credential_from_user_message,
)
from terminal_bench_pilot.probe_proxy_models import safe_error_metadata  # noqa: E402
from terminal_bench_pilot.scan_trial_evidence import contains_bytes  # noqa: E402
from terminal_bench_pilot.run_bounded_b_pair import (  # noqa: E402
    JobEventMonitor, LIVE_WRITE_SCOPES, PROVIDER_PROFILES, _clean_environment,
    _provider_credentials,
    _verify_binary_in_task_image, comparison_status, relay_usage_settled, parse_args,
    run_harbor_bounded,
    validate_process_grant,
)


class TerminalBenchPilotTests(unittest.TestCase):
    def test_proxy_error_probe_keeps_only_safe_machine_labels(self):
        secret = "sk-" + "x" * 64
        body = json.dumps({"error": {
            "code": "key_not_authorized",
            "type": "authentication_error",
            "message": "Bearer " + secret,
        }}).encode()
        metadata = safe_error_metadata(body)
        self.assertEqual(metadata, {
            "error_code": "key_not_authorized",
            "error_type": "authentication_error",
        })
        self.assertNotIn(secret, json.dumps(metadata))
        self.assertEqual(safe_error_metadata(b"<html>secret details</html>"), {})
        self.assertEqual(safe_error_metadata(json.dumps({"error": {
            "code": secret, "type": "access denied",
        }}).encode()), {})

    def test_pinaic_profile_uses_requested_protocol_reasoning_and_tariff(self):
        profile = PROVIDER_PROFILES["pinaic-gpt-6-luna"]
        self.assertEqual(profile["model_id"], "gpt-6-luna")
        self.assertEqual(profile["api_protocol"], "responses")
        self.assertEqual(profile["responses_reasoning_effort"], "max")
        self.assertEqual(profile["input_price_milli_usd_per_million"], 400)
        self.assertEqual(profile["cached_input_price_milli_usd_per_million"], 40)
        self.assertEqual(profile["output_price_milli_usd_per_million"], 2_000)
        peak_token_envelope = (
            profile["input_price_milli_usd_per_million"] * 16_000_000
            + profile["output_price_milli_usd_per_million"] * 440_000
        ) / 1_000_000_000
        self.assertEqual(peak_token_envelope, 7.28)

    def test_easycli_profile_uses_loopback_responses_without_account_key(self):
        profile = PROVIDER_PROFILES["easycli-local-gpt-6-luna"]
        self.assertEqual(profile["base_url"], "http://127.0.0.1:8317/v1")
        self.assertEqual(profile["model_id"], "gpt-6-luna")
        self.assertEqual(profile["api_protocol"], "responses")
        self.assertEqual(profile["responses_reasoning_effort"], "max")
        self.assertEqual(profile["relay_upstream_timeout_secs"], 600)
        self.assertEqual(profile["max_retryable_408_retries"], 1)
        self.assertGreater(
            profile["main_request_timeout_secs"],
            profile["relay_upstream_timeout_secs"],
        )
        self.assertTrue(profile["subscription_quota"])
        self.assertFalse(profile["proxy_price_verified"])
        with patch(
            "terminal_bench_pilot.run_bounded_b_pair.build_env",
            side_effect=AssertionError("must not open another provider configuration"),
        ):
            credentials = _provider_credentials(
                ROOT, profile, "gpt-6-luna", "responses", False,
            )
        self.assertEqual(credentials["OPENAI_API_KEY"], "")
        self.assertEqual(credentials["OPENAI_BASE_URL"], profile["base_url"])
        with self.assertRaisesRegex(RuntimeError, "cannot use a supplied key"):
            _provider_credentials(ROOT, profile, "gpt-6-luna", "responses", True)

    def test_credential_scan_detects_bytes_split_across_chunks(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "evidence.bin"
            key = b"sk-" + b"x" * 24
            path.write_bytes(b"a" * (1024 * 1024 - 2) + key + b"tail")
            self.assertTrue(contains_bytes(path, key))
            self.assertFalse(contains_bytes(path, b"absent-secret"))

    def test_session_launcher_reads_only_the_matching_user_message(self):
        key = "sk-" + "x" * 24
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "session.jsonl"
            rows = [
                {"type": "response_item", "payload": {"type": "message",
                    "role": "assistant", "content": [{"type": "output_text",
                    "text": "MiMo key " + key}]}},
                {"type": "response_item", "payload": {"type": "message",
                    "role": "user", "content": [{"type": "input_text",
                    "text": "switch to MiMo " + key}]}},
                {"type": "response_item", "payload": {"type": "message",
                    "role": "user", "content": [{"type": "input_text",
                    "text": "switch to MiMo latest " + "sk-" + "y" * 24 +
                            " sk-" + "y" * 24}]}},
            ]
            path.write_text("\n".join(json.dumps(row) for row in rows) + "\n",
                            encoding="utf-8")
            self.assertEqual(credential_from_user_message(path, "switch to MiMo latest"),
                             "sk-" + "y" * 24)
            self.assertEqual(credential_from_user_message(path, "switch to MiMo"),
                             "sk-" + "y" * 24)
            with self.assertRaisesRegex(RuntimeError, "expected exactly one"):
                credential_from_user_message(path, "unrelated request")

    def test_first_arm_runtime_failure_blocks_the_paid_second_arm(self):
        result = {"harbor_exit": 0, "runner_error": None, "trials": [{
            "runtime": {"status": "failed"}, "harbor_exception": "NonZeroAgentExitCodeError",
            "grading_status": "GRADED", "rewards": {"reward": 0.0},
        }]}
        self.assertEqual(comparison_status(result), "RUNTIME_NOT_COMPLETED")
        result["trials"][0]["runtime"]["status"] = "completed"
        result["trials"][0]["harbor_exception"] = None
        self.assertEqual(comparison_status(result), "COMPARABLE")
        result["trials"][0]["grading_status"] = "NO_GRADE"
        self.assertEqual(comparison_status(result), "NO_OFFICIAL_GRADE")

    def test_usage_unknown_blocks_the_comparison_even_with_a_grade(self):
        result = {"harbor_exit": 0, "runner_error": None, "trials": [{
            "runtime": {"status": "completed"}, "harbor_exception": None,
            "grading_status": "GRADED", "rewards": {"reward": 1.0},
        }]}
        self.assertEqual(comparison_status(result, provider_usage_settled=False), "USAGE_UNKNOWN")

    def test_usage_settled_accepts_only_the_exact_recovered_408_pair(self):
        relay = SimpleNamespace(
            active_calls=0,
            attempts=[
                {"state": "unknown", "retryable_upstream_408": True},
                {"state": "completed", "retry_of_unknown_408": True},
            ],
            retryable_408_recovered=True,
            input_tokens_reserved=4096,
            output_tokens_reserved=16,
        )
        self.assertTrue(relay_usage_settled(relay))
        relay.attempts[0]["retryable_upstream_408"] = False
        self.assertFalse(relay_usage_settled(relay))

    def test_new_window_requires_explicit_identity_and_stays_within_lock(self):
        argv = [
            "--report-name", "tb11-smoke-live-20260924",
            "--provider", "deepseek-flash", "--protocol", "TB11_SMOKE_LIVE_20260924",
            "--rounds", "24", "--attempts", "28", "--input-token-cap", "2000000",
            "--output-token-cap", "100000", "--peak-miss-usd-cap", "1",
            "--timeout-secs", "1200", "--expected-binary-sha256", "a" * 64,
            "--binary-path", "/tmp/frozen-agent-tui",
            "--grant-file", "/tmp/authorized-grant.json",
            "--expected-grant-sha256", "b" * 64,
            "--task-dir", "/tmp/locked-task",
            "--expected-image-sha256", "c" * 64,
            "--tool-attempt-cap", "1400",
            "--preflight-only",
        ]
        self.assertEqual(parse_args(argv).rounds, 24)
        single_file_argv = [
            "--report-name", "tb53-single-file-baseline-dynamic-56",
            "--protocol", "TB53_SINGLE_FILE_BASELINE_DYNAMIC_56_20260930",
            "--provider", "easycli-local-gpt-6-luna",
            "--arms", "dynamic", "--rounds", "56", "--attempts", "70",
            "--input-token-cap", "8000000", "--output-token-cap", "440000",
            "--peak-miss-usd-cap", "4", "--timeout-secs", "8400",
            "--expected-binary-sha256", "a" * 64,
            "--binary-path", "/tmp/agent-tui",
            "--grant-file", "/tmp/grants.json",
            "--expected-grant-sha256", "b" * 64,
            "--task-dir", "/tmp/task",
            "--expected-image-sha256", "c" * 64,
            "--tool-attempt-cap", "1400",
            "--standard-cyber-safeguards", "--single-file-edit-patches",
            "--preflight-only",
        ]
        parsed_single_file = parse_args(single_file_argv)
        self.assertTrue(parsed_single_file.standard_cyber_safeguards)
        self.assertTrue(parsed_single_file.single_file_edit_patches)
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            parse_args(argv[:argv.index("--protocol")] + argv[argv.index("--rounds"):])
        too_long = argv.copy()
        too_long[too_long.index("--rounds") + 1] = "401"
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            parse_args(too_long)
        too_expensive = argv.copy()
        too_expensive[too_expensive.index("--peak-miss-usd-cap") + 1] = "5"
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            parse_args(too_expensive)
        wrong_provider = single_file_argv.copy()
        wrong_provider[wrong_provider.index("--provider") + 1] = "deepseek-flash"
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            parse_args(wrong_provider)
        mimo = argv.copy()
        mimo[mimo.index("--provider") + 1] = "mimo-v2.6-flash"
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            parse_args(mimo)
        mimo.append("--credential-stdin")
        self.assertEqual(parse_args(mimo).provider, "mimo-v2.6-flash")
        mimo.extend(["--arms", "dynamic"])
        self.assertEqual(parse_args(mimo).arms, "dynamic")
        easycli = argv.copy()
        easycli[easycli.index("--provider") + 1] = "easycli-local-gpt-6-luna"
        self.assertEqual(parse_args(easycli).provider, "easycli-local-gpt-6-luna")
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            parse_args(easycli + ["--credential-stdin"])

    def test_binary_preflight_executes_the_exact_elf_in_the_locked_image(self):
        binary = Path("/tmp/checked-agent-tui")
        image_id = "sha256:" + "a" * 64
        ready = subprocess.CompletedProcess([], 0, stdout="--state-dir=PATH\n", stderr="")
        with patch("terminal_bench_pilot.run_bounded_b_pair.subprocess.run",
                   return_value=ready) as run:
            result = _verify_binary_in_task_image(binary, image_id)
        command = run.call_args.args[0]
        self.assertEqual(result["status"], "PASS")
        self.assertEqual(result["task_image_id"], image_id)
        self.assertIn("--network", command)
        self.assertEqual(command[command.index("--network") + 1], "none")
        self.assertIn("--read-only", command)
        self.assertIn("--pull=never", command)
        self.assertIn(f"src={binary}", command[command.index("--mount") + 1])
        self.assertIn("readonly", command[command.index("--mount") + 1])
        self.assertEqual(command[-2:], [image_id, "--help"])

    def test_binary_preflight_fails_closed_without_logging_loader_error(self):
        failed = subprocess.CompletedProcess(
            [], 1, stdout="", stderr="GLIBC_2.39 missing; synthetic secret",
        )
        with patch("terminal_bench_pilot.run_bounded_b_pair.subprocess.run",
                   return_value=failed):
            with self.assertRaisesRegex(RuntimeError, "cannot start") as caught:
                _verify_binary_in_task_image(Path("/tmp/old-elf"), "sha256:" + "b" * 64)
        self.assertNotIn("synthetic secret", str(caught.exception))

    def test_trial_environment_never_forwards_provider_account_keys(self):
        relay = SimpleNamespace(base_url="http://127.0.0.1:1/v1", token="trial-only")
        with patch.dict(os.environ, {
            "MIMO_API_KEY": "synthetic-account-secret",
            "XIAOMI_TOKEN": "synthetic-account-secret",
            "OPENAI_API_KEY": "synthetic-account-secret",
        }):
            env = _clean_environment(ROOT, relay, "mimo-v2.6-flash")
        self.assertNotIn("MIMO_API_KEY", env)
        self.assertNotIn("XIAOMI_TOKEN", env)
        self.assertNotIn("OPENAI_API_KEY", env)
        self.assertEqual(env["OPENAI_MODEL"], "mimo-v2.6-flash")
        self.assertEqual(env["TB_RELAY_TOKEN"], "trial-only")

    def test_job_monitor_stops_on_core_denial_without_double_counting_partial_lines(self):
        with tempfile.TemporaryDirectory() as directory:
            job_root = Path(directory)
            events = job_root / "live-database-cutover__trial/agent/context-agent/events.jsonl"
            events.parent.mkdir(parents=True)
            started = json.dumps({"event": {"type": "tool_started"}}) + "\n"
            denied = json.dumps({"event": {"type": "tool_finished", "output": {
                "metadata": {"failure_class": "approval_denied"},
            }}})
            events.write_text(started + denied[:-1], encoding="utf-8")
            monitor = JobEventMonitor(job_root, tool_cap=3)
            self.assertIsNone(monitor.scan())
            self.assertEqual(monitor.tool_attempts, 1)
            self.assertIsNone(monitor.scan())
            self.assertEqual(monitor.tool_attempts, 1)
            with events.open("a", encoding="utf-8") as stream:
                stream.write(denied[-1] + "\n")
            self.assertEqual(monitor.scan(), "ApprovalDeniedEarlyStop")
            self.assertEqual(monitor.tool_attempts, 1)

    def test_job_monitor_enforces_tool_attempt_threshold(self):
        with tempfile.TemporaryDirectory() as directory:
            job_root = Path(directory)
            events = job_root / "live-database-cutover__trial/agent/context-agent/events.jsonl"
            events.parent.mkdir(parents=True)
            events.write_text(
                (json.dumps({"event": {"type": "tool_started"}}) + "\n") * 2,
                encoding="utf-8",
            )
            monitor = JobEventMonitor(job_root, tool_cap=2)
            self.assertEqual(monitor.scan(), "ToolAttemptBudgetReached")
            self.assertEqual(monitor.tool_attempts, 2)

    @unittest.skipUnless(os.name == "posix", "SIGINT watchdog needs a POSIX child")
    def test_host_watchdog_interrupts_harbor_after_core_denial(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            job_root = root / "job"
            harbor = root / "fake-harbor"
            harbor.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, pathlib, time\n"
                "p = pathlib.Path(os.environ['FAKE_JOB_ROOT']) / "
                "'live-database-cutover__case/agent/context-agent/events.jsonl'\n"
                "p.parent.mkdir(parents=True)\n"
                "p.write_text(json.dumps({'event': {'type': 'tool_finished', "
                "'output': {'metadata': {'failure_class': 'approval_denied'}}}}) + '\\n')\n"
                "time.sleep(30)\n",
                encoding="utf-8",
            )
            harbor.chmod(0o755)
            started = time.monotonic()
            code, reason, attempts = run_harbor_bounded(
                str(harbor), root / "config.json", root,
                dict(os.environ, FAKE_JOB_ROOT=str(job_root)),
                root / "harbor.log", job_root, timeout_secs=15, tool_cap=1400,
            )
            self.assertEqual(reason, "ApprovalDeniedEarlyStop")
            self.assertEqual(attempts, 0)
            self.assertNotEqual(code, 0)
            self.assertLess(time.monotonic() - started, 10)

    def test_long_window_requires_a_matching_explicit_execution_grant(self):
        with tempfile.TemporaryDirectory() as directory:
            grant = Path(directory) / "grants.json"
            def write_grant(limit, expiry=2000):
                grant.write_text(json.dumps([{
                    "risk": "ProcessExecution",
                    "target": {"exec_argv_prefix": ["python3"]},
                    "constraint": {"max_runs": limit},
                    "expires_at_ms": expiry,
                }]), encoding="utf-8")

            write_grant(64)
            with self.assertRaisesRegex(RuntimeError, "64 runs for 400"):
                validate_process_grant(grant, 400)
            write_grant(400)
            validate_process_grant(grant, 400)
            validate_process_grant(grant, 400, min_expires_at_ms=1000)
            write_grant(400, expiry=900)
            with self.assertRaisesRegex(RuntimeError, "expires before"):
                validate_process_grant(grant, 400, min_expires_at_ms=1000)
            write_grant(None)
            with self.assertRaisesRegex(RuntimeError, "provide a separately authorized"):
                validate_process_grant(grant, 400)

    def test_live_grant_matches_declared_harness_write_scopes(self):
        grant = ROOT / "scripts/terminal_bench_pilot/grants_tb14_live.json"
        validate_process_grant(
            grant, 400, min_expires_at_ms=1000,
            expected_write_scopes=LIVE_WRITE_SCOPES,
        )
    def test_locked_selection_is_valid_and_budgeted(self):
        lock = json.loads(
            (ROOT / "docs/experiments/terminal-bench-pilot/selection.lock.json").read_text(
                encoding="utf-8"
            )
        )
        self.assertEqual(preflight.validate_lock(lock), [])
        self.assertEqual(lock["planned_primary_trials"], 12)

    def test_adapter_requires_explicit_authority_and_state_isolation(self):
        with self.assertRaises(harbor_agent.AdapterConfigurationError):
            harbor_agent.PilotAgentConfig().validate()
        with self.assertRaises(harbor_agent.AdapterConfigurationError):
            harbor_agent.PilotAgentConfig(
                grant_file="/etc/context-agent/grants.json",
                state_dir="/tmp/context-agent-state",
                runtime_supports_state_dir=False,
            ).validate()

    def test_command_has_both_context_and_finite_budget(self):
        config = harbor_agent.PilotAgentConfig(
            context="dynamic",
            max_rounds=17,
            timeout_secs=99,
            grant_file="/etc/context-agent/grants.json",
        )
        argv = harbor_agent.build_agent_argv(config)
        self.assertIn("--context=dynamic", argv)
        self.assertIn("--max-rounds=17", argv)
        self.assertIn("--timeout-secs=99", argv)
        self.assertTrue(any(item.startswith("--task-goal=") for item in argv))
        self.assertEqual(argv[-1], "/app")

    def test_harbor_entry_point_keeps_isolated_state_defaults(self):
        agent = harbor_agent.ContextAgentTerminalBench(
            grant_file="/etc/context-agent/grants.json"
        )
        agent.config.validate()
        self.assertEqual(agent.config.state_dir, "/tmp/context-agent-run/state")
        self.assertTrue(agent.config.runtime_supports_state_dir)

        demo = harbor_agent.ContextAgentTerminalBench(
            grant_file="/etc/context-agent/grants.json", demo="true"
        )
        self.assertTrue(demo.config.demo)

        provider = harbor_agent.ContextAgentTerminalBench(
            grant_file="/etc/context-agent/grants.json",
            extra_env={"OPENAI_MODEL": "deepseek-flash"},
        )
        self.assertEqual(provider._extra_env, {"OPENAI_MODEL": "deepseek-flash"})

    def test_prompt_is_shell_quoted_as_data(self):
        config = harbor_agent.PilotAgentConfig(grant_file="/tmp/g.json")
        command = harbor_agent.build_prompt_pipeline("line 1; rm -rf /\nline 2", config)
        self.assertIn("MAINTENANCE_MAX_CALLS_PER_MAINTAIN=0", command)
        self.assertIn("MAINTENANCE_MAX_TOKENS_PER_MAINTAIN=0", command)
        self.assertIn("| MAINTENANCE_MAX_CALLS_PER_MAINTAIN=0", command)
        self.assertIn("printf '%s'", command)
        self.assertIn("rm -rf /", command)
        self.assertNotIn("printf '%s' line 1;", command)
        self.assertIn("entrypoint.sh", command)
        self.assertIn("migrate_archive/", command)
        self.assertIn("same single Core write grant", command)
        self.assertIn("api/main.py and migrate.py", command)
        self.assertIn("non-empty executable name", command)

    def test_task_scoped_single_file_patch_guard_is_prompted(self):
        config = harbor_agent.PilotAgentConfig(
            grant_file="/tmp/g.json", single_file_edit_patches=True,
        )
        command = harbor_agent.build_prompt_pipeline("solve task", config)
        self.assertIn("every edit.patch call must contain exactly one file", command)
        self.assertIn("The host sends this trial with a strict one-file schema", command)

    def test_preflight_is_supplier_free_and_reports_current_blockers(self):
        report = preflight.run_preflight()
        self.assertEqual(report["supplier_calls"], 0)
        self.assertFalse(report["provider_contacted"])
        self.assertIn(report["status"], {"STATIC_READY", "NOT_READY"})
        self.assertEqual(report["functional_smoke"], "NOT_RUN")
        self.assertFalse(report["trial_ready"])
        self.assertIn("selection_lock", report["checks"])
        self.assertIn("artifact_state_isolation", report["checks"])

    def test_preflight_considers_debian12_linux_build(self):
        candidates = [
            str(item["path"])
            for item in preflight._find_binary()[1]
        ]
        self.assertTrue(any("target-debian12" in path for path in candidates))

    def test_static_preflight_cannot_authorize_a_trial(self):
        self.assertFalse(preflight.run_preflight()["trial_ready"])


if __name__ == "__main__":
    unittest.main()
