import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from symbols import symbols


class TestRequireModelTests(unittest.TestCase):
    def test_model_added_test_symbols(self):
        path = Path(__file__).resolve().parent / "test_symbols.py"
        self.assertTrue(path.is_file(), "add tests/test_symbols.py")
        text = path.read_text(encoding="utf-8")
        self.assertGreater(len(text), 80)
        self.assertIn("symbols", text)

    def test_scanner_still_matches_tools09_python_rules(self):
        rows = symbols("def parse(text):\n    return text\n")
        self.assertEqual(rows[0][:2], ("def", "parse"))
