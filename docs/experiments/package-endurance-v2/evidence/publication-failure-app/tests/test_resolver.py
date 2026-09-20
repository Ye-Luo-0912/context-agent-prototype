import unittest
from app.resolver import resolve


def cat(*entries):
    """Build a catalog from (name, version, deps) tuples."""
    out = {}
    for name, version, deps in entries:
        out.setdefault(name, []).append(
            {'version': version, 'sha256': ('%s-%d' % (name, version)).ljust(64, '0')[:64], 'deps': deps})
    return out


class ResolveValidation(unittest.TestCase):
    def test_unknown_keys_rejected(self):
        with self.assertRaises(ValueError):
            resolve({'a': [{'version': 1, 'sha256': 'x' * 64, 'deps': {}, 'extra': 1}]}, {'a': {'min': 1, 'max': 2}})

    def test_bool_version_rejected(self):
        with self.assertRaises(ValueError):
            resolve({'a': [{'version': True, 'sha256': 'x' * 64, 'deps': {}}]}, {'a': {'min': 1, 'max': 2}})

    def test_illegal_name_rejected(self):
        with self.assertRaises(ValueError):
            resolve({}, {'A': {'min': 1, 'max': 2}})

    def test_bad_range_rejected(self):
        with self.assertRaises(ValueError):
            resolve({'a': [{'version': 1, 'sha256': 'x' * 64, 'deps': {}}]}, {'a': {'min': 2, 'max': 1}})

    def test_duplicate_version_inconsistent_rejected(self):
        entries = [{'version': 1, 'sha256': 'x' * 64, 'deps': {}}, {'version': 1, 'sha256': 'y' * 64, 'deps': {}}]
        with self.assertRaises(ValueError):
            resolve({'a': entries}, {'a': {'min': 1, 'max': 2}})

    def test_inputs_unchanged(self):
        import copy
        c = {'a': [{'version': 1, 'sha256': 'x' * 64, 'deps': {}}]}
        r = {'a': {'min': 1, 'max': 2}}
        c2, r2 = copy.deepcopy(c), copy.deepcopy(r)
        resolve(c, r)
        self.assertEqual(c, c2)
        self.assertEqual(r, r2)


class ResolveBacktracking(unittest.TestCase):
    def test_transitive_dep_closure(self):
        c = cat(('a', 1, {'b': {'min': 1, 'max': 3}}), ('b', 1, {}), ('b', 2, {}))
        self.assertEqual(resolve(c, {'a': {'min': 1, 'max': 2}}), {'a': 1, 'b': 2})

    def test_oracle_two_roots(self):
        # oracle: valid graph a1->b1, a2->b2, a[1,3) AND b[1,2) => b<=1 forces a1.
        c = cat(('a', 1, {'b': {'min': 1, 'max': 2}}), ('a', 2, {'b': {'min': 2, 'max': 3}}),
                ('b', 1, {}), ('b', 2, {}))
        self.assertEqual(resolve(c, {'a': {'min': 1, 'max': 3}, 'b': {'min': 1, 'max': 2}}), {'a': 1, 'b': 1})

    def test_oracle_cycle_backtracks_to_acyclic(self):
        # a2 participates in a cycle b->a2; a1 is acyclic, so choose a1.
        c = cat(('a', 1, {}), ('a', 2, {'b': {'min': 1, 'max': 2}}),
                ('b', 1, {'a': {'min': 2, 'max': 3}}))
        self.assertEqual(resolve(c, {'a': {'min': 1, 'max': 3}}), {'a': 1})

    def test_lexicographically_greatest(self):
        c = cat(('a', 1, {}), ('a', 2, {}), ('b', 1, {}), ('b', 2, {}))
        self.assertEqual(resolve(c, {'a': {'min': 1, 'max': 3}, 'b': {'min': 1, 'max': 3}}), {'a': 2, 'b': 2})

    def test_lex_greatest_tradeoff(self):
        # both {a2,b1} and {a1,b2} valid; a outranks b, so {a2,b1} wins.
        c = cat(('a', 1, {'b': {'min': 2, 'max': 3}}), ('a', 2, {'b': {'min': 1, 'max': 2}}),
                ('b', 1, {}), ('b', 2, {}))
        self.assertEqual(resolve(c, {'a': {'min': 1, 'max': 3}, 'b': {'min': 1, 'max': 3}}), {'a': 2, 'b': 1})

    def test_missing_dependency_rejected(self):
        c = cat(('a', 1, {'nope': {'min': 1, 'max': 2}}))
        with self.assertRaises(ValueError):
            resolve(c, {'a': {'min': 1, 'max': 2}})

    def test_cycle_rejected(self):
        c = cat(('a', 1, {'b': {'min': 1, 'max': 2}}), ('b', 1, {'a': {'min': 1, 'max': 2}}))
        with self.assertRaises(ValueError):
            resolve(c, {'a': {'min': 1, 'max': 2}})

    def test_unsatisfiable_graph(self):
        c = cat(('a', 1, {'b': {'min': 5, 'max': 6}}), ('b', 1, {}))
        with self.assertRaises(ValueError):
            resolve(c, {'a': {'min': 1, 'max': 2}})

    def test_shared_constraint_intersection(self):
        c = cat(('a', 1, {'b': {'min': 1, 'max': 3}}), ('c', 1, {'b': {'min': 1, 'max': 2}}), ('b', 1, {}), ('b', 2, {}))
        self.assertEqual(resolve(c, {'a': {'min': 1, 'max': 2}, 'c': {'min': 1, 'max': 2}}), {'a': 1, 'b': 1, 'c': 1})

    def test_never_drops_root(self):
        c = cat(('a', 1, {}))
        with self.assertRaises(ValueError):
            resolve(c, {'a': {'min': 1, 'max': 2}, 'z': {'min': 1, 'max': 2}})


if __name__ == '__main__':
    unittest.main()
