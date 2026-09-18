"""Paid-run follow-up regressions: child command path resolution and
summary extraction from the headless event journal.

- Found live: a relative --campaign-dir produced a relative --grant-file
  argument the child (cwd=workspace) could not resolve, and the child died
  at startup before any request (runner correctly reported child_nonzero/10
  with zero spend; the path handling is what this pins).
- Found live: summary rounds/tool_calls/terminals counted zero against 14
  real model rounds because the journal wraps events in envelope rows and
  the extraction never unwrapped them.
"""
from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

import _support as support

runner = support.runner


class SummaryEnvelopeTests(unittest.TestCase):
    def test_summary_counts_unwrap_event_envelopes(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            campaign = support.make_campaign(root / "campaign")
            rows = [
                {"run_id": "r", "seq": 1, "event": {"type": "run_started"}},
                {"run_id": "r", "seq": 2, "event": {"type": "model_started"}},
                {"run_id": "r", "seq": 3, "event": {"type": "tool_finished"}},
                {"run_id": "r", "seq": 4, "event": {"type": "recovery_required"}},
                {"run_id": "r", "seq": 5, "event": {"type": "turn_failed"}},
            ]
            payload = "\n".join(json.dumps(row) for row in rows)
            child = (
                "import os\n"
                "path = os.environ['STUB_EVENTS_PATH']\n"
                f"text = {payload!r}\n"
                "with open(path, 'w', encoding='utf-8') as handle:\n"
                "    handle.write(text)\n"
            )
            env = support.minimal_env("http://127.0.0.1:9")
            # the runner reads <segment dir>/events.jsonl; the seg dir is
            # created before the child spawns, so the stub can fill it
            env["STUB_EVENTS_PATH"] = str(campaign / "seg-001" / "events.jsonl")
            cfg = support.base_config(campaign, env=env, child_command=[sys.executable, "-c", child])
            code = runner.run_segment(cfg)
            self.assertEqual(code, runner.EXIT_RECOVERY_REQUIRED)
            summary = support.read_json(campaign / "seg-001" / "summary.json")
            self.assertEqual(summary["rounds"], 1)
            self.assertEqual(summary["tool_calls"], 1)
            self.assertTrue(any(t.get("type") == "recovery_required" for t in summary["terminals"]))
            self.assertIn("recovery_required", summary["categories"])


class RelativeCampaignDirTests(unittest.TestCase):
    def test_relative_campaign_dir_yields_absolute_child_paths(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            support.make_campaign(root / "campaign")
            previous = os.getcwd()
            os.chdir(root)
            try:
                cfg = support.base_config(
                    Path("campaign"),  # constructed relative on purpose
                    child_command=None,  # force the real default command builder
                )
                cfg.campaign_dir = "campaign"  # a caller may spell it relative
                cfg.work_dir = None  # must derive from the resolved campaign dir
                code = runner.run_segment(cfg)
            finally:
                os.chdir(previous)
            # The default binary is python.exe, which rejects the agent flags
            # and exits nonzero; the launch itself succeeds, so the recorded
            # child_command is the thing under test.
            self.assertEqual(code, runner.EXIT_CHILD_FAILED)
            metadata = support.read_json(root / "campaign" / "seg-001" / "metadata.json")
            grant = Path([a.split("=", 1)[1] for a in metadata["child_command"] if a.startswith("--grant-file=")][0])
            events = Path([a.split("=", 1)[1] for a in metadata["child_command"] if a.startswith("--jsonl-out=")][0])
            self.assertTrue(grant.is_absolute(), metadata["child_command"])
            self.assertTrue(grant.is_file(), metadata["child_command"])
            self.assertTrue(events.is_absolute(), metadata["child_command"])
            self.assertEqual(grant.parent, (root / "campaign" / "seg-001").resolve())


if __name__ == "__main__":
    unittest.main()
