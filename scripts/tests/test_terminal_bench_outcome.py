import json
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from terminal_bench_pilot.outcome import summarize_trial


class OutcomeTests(unittest.TestCase):
    def test_budget_finalization_is_reported_from_the_last_typed_surface(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            events = path / "agent/context-agent/events.jsonl"
            events.parent.mkdir(parents=True)
            events.write_text("\n".join(json.dumps(row) for row in (
                {"event": {"type": "tool_surface_planned", "report": {
                    "model_round": 56, "selected": [],
                    "omitted": [{"tool_name": "edit.patch",
                                 "reason": "decision_budget_finalization"}],
                }}},
                {"event": {"type": "model_used", "input_tokens": 10,
                           "output_tokens": 2}},
                {"kind": "session_end", "exit": 0, "status": "completed",
                 "round_budget": False, "task_completed": False},
            )) + "\n", encoding="utf-8")
            summary = summarize_trial(path)
            self.assertEqual(summary["runtime"]["finalization_reason"],
                             "decision_budget_finalization")
            self.assertFalse(summary["runtime"]["round_budget"])

    def test_planned_budget_finalization_without_model_use_is_not_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            events = path / "agent/context-agent/events.jsonl"
            events.parent.mkdir(parents=True)
            events.write_text("\n".join(json.dumps(row) for row in (
                {"event": {"type": "tool_surface_planned", "report": {
                    "selected": [], "omitted": [
                        {"reason": "decision_budget_finalization"}],
                }}},
                {"kind": "session_end", "exit": 0, "status": "completed",
                 "round_budget": False, "task_completed": False},
            )) + "\n", encoding="utf-8")
            self.assertIsNone(summarize_trial(path)["runtime"]["finalization_reason"])

    def test_later_normal_surface_clears_earlier_budget_finalization(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            events = path / "agent/context-agent/events.jsonl"
            events.parent.mkdir(parents=True)
            events.write_text("\n".join(json.dumps(row) for row in (
                {"event": {"type": "tool_surface_planned", "report": {
                    "selected": [], "omitted": [
                        {"reason": "decision_budget_finalization"}],
                }}},
                {"event": {"type": "tool_surface_planned", "report": {
                    "selected": [{"tool_name": "fs.read"}], "omitted": [],
                }}},
                {"kind": "session_end", "exit": 0, "status": "completed",
                 "round_budget": False, "task_completed": False},
            )) + "\n", encoding="utf-8")
            self.assertIsNone(summarize_trial(path)["runtime"]["finalization_reason"])

    def test_denial_exit_and_grade_are_independent(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)
            (path / "agent/context-agent").mkdir(parents=True)
            (path / "agent/context-agent/events.jsonl").write_text(json.dumps({
                "kind": "session_end", "exit": 3, "status": "approval_denied"}), encoding="utf-8")
            (path / "result.json").write_text(json.dumps({
                "exception_info": {"exception_type": "NonZeroAgentExitCodeError",
                                   "exception_message": "sensitive text must not be copied"},
                "verifier_result": {"rewards": {"reward": 0.0}}}), encoding="utf-8")
            result = summarize_trial(path)
            self.assertEqual(result["grading_status"], "GRADED")
            self.assertEqual(result["runtime"]["exit"], 3)
            self.assertEqual(result["rewards"], {"reward": 0.0})
            self.assertNotIn("sensitive text", json.dumps(result))


if __name__ == "__main__":
    unittest.main()
