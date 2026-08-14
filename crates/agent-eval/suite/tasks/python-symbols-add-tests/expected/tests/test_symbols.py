import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from symbols import symbols


class TestSymbols(unittest.TestCase):
    def test_def_class_and_async_def(self):
        source = (
            "def parse(text):\n"
            "    return text\n"
            "class Parser:\n"
            "    pass\n"
            "async def fetch():\n"
            "    pass\n"
        )
        rows = symbols(source)
        self.assertEqual(
            rows,
            [("def", "parse", 1), ("class", "Parser", 3), ("def", "fetch", 5)],
        )

    def test_commented_def_is_skipped(self):
        rows = symbols("# def fake():\npass\n")
        self.assertEqual(rows, [])
