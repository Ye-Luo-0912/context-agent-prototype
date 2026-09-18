"""The spawned child runs with cwd=workspace, so a relative --campaign-dir
must never leak relative paths into the child command.

Found live on a paid run: a relative --campaign-dir produced a relative
--grant-file argument the child could not resolve from its own cwd, and
the child died at startup before any request (runner correctly reported
child_nonzero/10 with zero spend; the path handling is what this pins).
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
