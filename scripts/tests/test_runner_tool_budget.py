"""Campaign-level tool budget: the runner stops a child that keeps spending.

The frozen V5 campaign caps tool attempts per campaign, not per segment. These
tests prove the in-flight enforcement (a chatty child is stopped at the bound)
and that the previous behaviour is untouched when no budget is configured.
"""
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

import _support as support

runner = support.runner

CHILD_CHATTY = (
    "import json, sys, time\n"
    "from pathlib import Path\n"
    "path = Path(sys.argv[1])\n"
    "count = int(sys.argv[2])\n"
    "path.parent.mkdir(parents=True, exist_ok=True)\n"
    "for index in range(count):\n"
    "    row = {'run_id': 'r', 'seq': index + 1, 'event': {'type': 'tool_finished'}}\n"
    "    with path.open('a', encoding='utf-8') as handle:\n"
    "        handle.write(json.dumps(row) + '\\n')\n"
    "    time.sleep(0.05)\n"
    "time.sleep(30)\n"
)


class ToolBudgetWatcher(unittest.TestCase):
    def test_watcher_counts_incrementally_and_unwraps_the_journal_envelope(self):
        with tempfile.TemporaryDirectory() as raw:
            path = Path(raw) / "events.jsonl"
            watcher = runner._ToolBudgetWatcher(path, budget=3)
            self.assertIsNone(watcher.poll(), "no journal yet")
            path.write_text(
                json.dumps(dict(run_id="r", seq=1, event=dict(type="model_started"))) + "\n"
                + json.dumps(dict(run_id="r", seq=2, event=dict(type="tool_finished"))) + "\n",
                encoding="utf-8")
            self.assertIsNone(watcher.poll())
            self.assertEqual(watcher.snapshot()["segment_used"], 1)
            with path.open("a", encoding="utf-8") as handle:
                handle.write(json.dumps(dict(run_id="r", seq=3, event=dict(type="tool_finished"))) + "\n")
                handle.write(json.dumps(dict(run_id="r", seq=4, event=dict(type="tool_finished"))) + "\n")
            self.assertEqual(watcher.poll(), "tool_budget_exhausted")
            self.assertTrue(watcher.snapshot()["exhausted"])
            # A partial trailing line must not be counted twice or lost.
            with path.open("a", encoding="utf-8") as handle:
                handle.write('{"run_id": "r", "seq": 5, "event": {"type": "tool_fin')
            watcher.poll()
            with path.open("a", encoding="utf-8") as handle:
                handle.write('ished"}}\n')
            watcher.poll()
            self.assertEqual(watcher.snapshot()["segment_used"], 4)

    def test_baseline_is_consumed_from_the_campaign_not_re_granted(self):
        watcher = runner._ToolBudgetWatcher(Path("unused.jsonl"), budget=8, baseline=8)
        self.assertEqual(watcher.poll(), "tool_budget_exhausted")
        self.assertEqual(watcher.snapshot()["segment_used"], 0)


class InFlightEnforcement(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.campaign = support.make_campaign(self.root / "campaign")

    def run_child(self, **overrides):
        out_dir = self.campaign / "seg-001"
        stub = support.stub_file(self.root, "chatty.py", CHILD_CHATTY)
        overrides.setdefault("child_command",
                             [sys.executable, stub, str(out_dir / "events.jsonl"), "8"])
        overrides.setdefault("child_wait_timeout_s", 20.0)
        overrides.setdefault("rounds", 1)
        cfg = support.base_config(self.campaign, **overrides)
        code = runner.run_segment(cfg)
        summary = json.loads((out_dir / "summary.json").read_bytes())
        return code, summary

    def test_chatty_child_is_stopped_at_the_campaign_tool_budget(self):
        code, summary = self.run_child(tool_budget=3, tool_budget_baseline=0)
        self.assertEqual(code, runner.EXIT_TOOL_BUDGET)
        self.assertEqual(summary["outcome"], "tool_budget_exhausted")
        self.assertIn("tool_budget_exhausted", summary["categories"])
        self.assertTrue(summary["tool_budget"]["exhausted"])
        self.assertGreaterEqual(summary["tool_budget"]["used"], 3)

    def test_absent_budget_keeps_the_previous_behaviour(self):
        code, summary = self.run_child(child_wait_timeout_s=1.0)
        self.assertEqual(code, runner.EXIT_CHILD_TIMEOUT)
        self.assertIsNone(summary["tool_budget"], "no budget configured, no in-flight accounting")


if __name__ == "__main__":
    unittest.main()
