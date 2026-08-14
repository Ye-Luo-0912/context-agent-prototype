import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from remove_prefix import remove_prefix


class TestRemovePrefix(unittest.TestCase):
    def test_literal_prefix_not_charset(self):
        self.assertEqual(remove_prefix("test", "te"), "st")
        self.assertEqual(remove_prefix("test", "t"), "est")
        self.assertEqual(remove_prefix("www.example.com", "w"), "ww.example.com")
        self.assertEqual(remove_prefix("tmp_alpha", "tmp_"), "alpha")
        self.assertEqual(remove_prefix("tmp_tmp_nested", "tmp_"), "tmp_nested")

    def test_missing_prefix_is_unchanged(self):
        self.assertEqual(remove_prefix("beta", "tmp_"), "beta")
        self.assertEqual(remove_prefix("test", "x"), "test")

    def test_empty_prefix_is_a_copy(self):
        self.assertEqual(remove_prefix("test", ""), "test")
