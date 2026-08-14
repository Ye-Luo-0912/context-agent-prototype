import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from batched import batched


class TestBatched(unittest.TestCase):
    def test_groups_and_short_tail(self):
        self.assertEqual(
            list(batched("ABCDEFG", 3)),
            [["A", "B", "C"], ["D", "E", "F"], ["G"]],
        )
        self.assertEqual(
            list(batched("ABCDEFG", 2)),
            [["A", "B"], ["C", "D"], ["E", "F"], ["G"]],
        )
        self.assertEqual(list(batched("ABCDEFG", 1)), [[ch] for ch in "ABCDEFG"])
        self.assertEqual(list(batched("", 3)), [])

    def test_rejects_non_positive_n(self):
        with self.assertRaises(ValueError):
            list(batched("ABCDEFG", 0))
        with self.assertRaises(ValueError):
            list(batched("ABCDEFG", -1))

    def test_preserves_order(self):
        data = "ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        for n in range(1, 6):
            batches = list(batched(data, n))
            self.assertEqual("".join("".join(batch) for batch in batches), data)
            if batches:
                last = batches[-1]
                self.assertTrue(all(len(batch) == n for batch in batches[:-1]))
                self.assertLessEqual(len(last), n)
