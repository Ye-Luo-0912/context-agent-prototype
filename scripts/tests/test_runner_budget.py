"""BR6 regressions: persistent budget ledger, reserve-before-accept, strict usage."""
from __future__ import annotations

import io
import contextlib
import json
import sys
import tempfile
import unittest
from pathlib import Path

import _support as support

runner = support.runner


def expected_usage_cost() -> float:
    pricing = runner.Pricing()
    return pricing.attempt_cost_usd(1000, 0, 100)


def expected_reserve(max_output_tokens: int = 8192) -> float:
    return runner.Pricing().reserve_estimate_usd(len(support.REQUEST_BODY), max_output_tokens)


class RunnerBudgetTests(unittest.TestCase):
    def test_full_usage_settles_committed(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("ok") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, 0)
            ledger = support.read_json(campaign / "budget-ledger.json")
            self.assertAlmostEqual(ledger["committed_usd"], expected_usage_cost(), places=12)
            self.assertAlmostEqual(ledger["reserved_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)
            self.assertFalse(ledger["cap_stopped"])
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["id"], 1)
            self.assertEqual(attempt["request"], 1)
            self.assertEqual(attempt["status"], "committed")
            out = campaign / "seg-001"
            usage_ledger = support.read_json(out / "usage-ledger.json")
            self.assertTrue(usage_ledger["amounts_are_estimates"])
            self.assertEqual(usage_ledger["rows"][0]["status"], "committed")
            self.assertIn("reserve_policy", usage_ledger)
            metadata = support.read_json(out / "metadata.json")
            self.assertFalse(metadata["budget"]["inherited_ledger"])
            self.assertIn("reserve_policy", metadata["budget"])

    def test_missing_usage_field_settles_unknown_without_zero_fill(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("missing_cached") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / "budget-ledger.json")
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "unknown")
            # the reserved estimate stays occupied as unknown, never guessed to zero
            self.assertAlmostEqual(ledger["unknown_usd"], expected_reserve(), places=12)
            self.assertNotIn("input_tokens", attempt)
            self.assertNotIn("output_tokens", attempt)
            self.assertIn("cached_input_tokens", attempt["detail"])
            self.assertEqual(len(upstream.requests), 1)

    def test_nested_details_usage_shape_settles_committed(self):
        # DeepSeek's Responses-compatible serving reports the cache-hit
        # bucket as input_tokens_details.cached_tokens (observed live on the
        # 2026-09-19 paid run); it must settle committed, never unknown.
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("nested_cached") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, 0)
            ledger = support.read_json(campaign / "budget-ledger.json")
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "committed")
            self.assertEqual(attempt["usage_shape"], "input_tokens_details.cached_tokens")
            self.assertEqual(attempt["input_tokens"], 1000)
            self.assertEqual(attempt["cached_input_tokens"], 40)
            expected = runner.Pricing().attempt_cost_usd(1000, 40, 100)
            self.assertAlmostEqual(ledger["committed_usd"], expected, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)
            self.assertEqual(len(upstream.requests), 1)

    def test_parse_usage_accepts_nested_and_flat_cache_shapes(self):
        nested, reason = runner.parse_usage_strict(
            support._sse({"input_tokens": 10, "input_tokens_details": {"cached_tokens": 4}, "output_tokens": 2})
        )
        self.assertIsNone(reason)
        self.assertEqual(nested["cached_input_tokens"], 4)
        self.assertEqual(nested["usage_shape"], "input_tokens_details.cached_tokens")
        flat, reason = runner.parse_usage_strict(support._sse(dict(support.USAGE_FULL)))
        self.assertIsNone(reason)
        self.assertEqual(flat["cached_input_tokens"], 0)
        self.assertEqual(flat["usage_shape"], "cached_input_tokens")
        missing, reason = runner.parse_usage_strict(support._sse({"input_tokens": 10, "output_tokens": 2}))
        self.assertIsNone(missing)
        self.assertIn("cached_input_tokens", reason)

    def test_reserve_rejects_request_over_cap_before_forwarding(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("ok") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    max_cost_usd=0.005,  # below a single reserve estimate (~0.0099)
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            self.assertEqual(upstream.requests, [], "over-cap request must be refused before upstream contact")
            ledger = support.read_json(campaign / "budget-ledger.json")
            self.assertTrue(ledger["cap_stopped"])
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "rejected_cap")
            self.assertEqual(attempt["settled_usd"], 0.0)

    def test_rejected_attempts_are_not_billed(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("ok") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    max_cost_usd=0.005,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "2", "0.3"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / "budget-ledger.json")
            statuses = [attempt["status"] for attempt in ledger["attempts"]]
            self.assertEqual(statuses, ["rejected_cap", "rejected_cap"])
            self.assertAlmostEqual(ledger["committed_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["reserved_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)
            self.assertEqual(upstream.requests, [], "429 rejections must not be forwarded nor double-billed")

    def test_concurrent_reserve_mutual_exclusion(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            # first request stalls ~2.5s upstream, second fires after 1.2s while
            # the first reservation is still open; the cap only fits one.
            cap = expected_reserve() * 1.5
            with support.StubUpstream("delay", delay_s=2.5) as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    max_cost_usd=cap,
                    upstream_timeout_s=10.0,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "2", "1.2"],
                )
                code = runner.run_segment(cfg)
            ledger = support.read_json(campaign / "budget-ledger.json")
            statuses = sorted(attempt["status"] for attempt in ledger["attempts"])
            self.assertEqual(statuses, ["committed", "rejected_cap"])
            self.assertAlmostEqual(ledger["committed_usd"], expected_usage_cost(), places=12)
            self.assertAlmostEqual(ledger["reserved_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)

    def test_ledger_inherited_across_processes(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            # est(max_output_tokens=100) ~ 2.0e-4; cap leaves 1.8e-4 after run 1,
            # so the second process's reservation must be refused.
            cap = 0.0006
            with support.StubUpstream("ok") as upstream:
                first = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    segment="seg-001",
                    max_cost_usd=cap,
                    max_output_tokens=100,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code_first = runner.run_segment(first)
                second = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    segment="seg-002",
                    max_cost_usd=cap,
                    max_output_tokens=100,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code_second = runner.run_segment(second)
            self.assertEqual(code_first, 0)
            self.assertEqual(code_second, runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / "budget-ledger.json")
            self.assertAlmostEqual(ledger["committed_usd"], expected_usage_cost(), places=12)
            self.assertEqual([attempt["id"] for attempt in ledger["attempts"]], [1, 2], "attempt ids stay monotonic across processes")
            self.assertEqual(ledger["attempts"][0]["segment"], "seg-001")
            self.assertEqual(ledger["attempts"][1]["segment"], "seg-002")
            self.assertEqual(ledger["attempts"][1]["status"], "rejected_cap")
            self.assertEqual(len(upstream.requests), 1, "second process's refused request must not reach upstream")
            metadata_second = support.read_json(campaign / "seg-002" / "metadata.json")
            self.assertTrue(metadata_second["budget"]["inherited_ledger"])
            self.assertTrue(metadata_second["budget"]["cap_stopped"])

    def test_boundary_last_request_rejected(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            pricing = runner.Pricing()
            # remaining budget 0.01: an estimate of ~0.0097 fits, 0.0201 does not
            fitting, _ = runner.BudgetLedger.load(root / "ledger-a.json", 0.03, pricing, 8000)
            fitting.data["committed_usd"] = 0.02
            attempt_id, estimate, rejected = fitting.reserve(1, 40, "seg-b")
            self.assertFalse(rejected)
            self.assertAlmostEqual(estimate, 0.0096798, places=9)
            self.assertIsNotNone(attempt_id)
            oversized, _ = runner.BudgetLedger.load(root / "ledger-b.json", 0.03, pricing, 16666)
            oversized.data["committed_usd"] = 0.02
            attempt_id, estimate, rejected = oversized.reserve(1, 40, "seg-b")
            self.assertTrue(rejected, "estimate 0.0201 must be refused with 0.01 remaining")
            self.assertAlmostEqual(estimate, 0.020079, places=9)
            self.assertTrue(oversized.data["cap_stopped"])

    def test_upstream_stall_settles_unknown_without_crash(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("stall", stall_s=30.0) as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    upstream_timeout_s=0.8,
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            ledger = support.read_json(campaign / "budget-ledger.json")
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "unknown")
            self.assertTrue(attempt["detail"].startswith("upstream_failure:"))
            self.assertAlmostEqual(ledger["unknown_usd"], expected_reserve(), places=12)
            summary = support.read_json(campaign / "seg-001" / "summary.json")
            self.assertEqual(summary["outcome"], "completed", "runner must not crash on a hung upstream")

    def test_upstream_429_not_billed(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            with support.StubUpstream("rate_limited") as upstream:
                cfg = support.base_config(
                    campaign,
                    env=support.minimal_env(upstream.url),
                    child_command=[sys.executable, support.stub_file(root, "request.py", support.CHILD_REQUEST), "1"],
                )
                code = runner.run_segment(cfg)
            self.assertEqual(code, 0)
            ledger = support.read_json(campaign / "budget-ledger.json")
            attempt = ledger["attempts"][0]
            self.assertEqual(attempt["status"], "upstream_rate_limited")
            self.assertEqual(attempt["settled_usd"], 0.0)
            self.assertAlmostEqual(ledger["committed_usd"], 0.0, places=12)
            self.assertAlmostEqual(ledger["unknown_usd"], 0.0, places=12)
            self.assertFalse(ledger["cap_stopped"])

    def test_corrupt_ledger_refuses_to_run(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            (campaign / "budget-ledger.json").write_text("{broken", encoding="utf-8")
            cfg = support.base_config(campaign, child_command=[sys.executable, support.stub_file(root, "exit0.py", support.CHILD_EXIT_0)])
            stdout = io.StringIO()
            with contextlib.redirect_stdout(stdout):
                code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_BUDGET_INCOMPLETE)
            self.assertFalse((campaign / "seg-001").exists(), "must not start a segment on an unreadable ledger")
            status = json.loads(stdout.getvalue().strip().splitlines()[-1])
            self.assertEqual(status["status"], "budget_ledger_unreadable")

    def test_cli_rejects_invalid_budget_numbers(self):
        cases = [
            ["seg-001", "--max-cost-usd=nan"],
            ["seg-001", "--max-cost-usd=inf"],
            ["seg-001", "--max-cost-usd=-0.01"],
            ["seg-001", "--rounds=-3"],
            ["seg-001", "--max-output-tokens=-5"],
        ]
        for argv in cases:
            with self.subTest(argv=argv):
                self.assertEqual(runner.main(argv), 2)


if __name__ == "__main__":
    unittest.main()
