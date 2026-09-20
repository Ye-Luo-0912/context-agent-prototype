"""Deterministic dependency resolver with genuine backtracking (SPEC.md).

`resolve(catalog, requirements)` returns the lexicographically greatest
version vector (by ascending package name, then highest version) that
satisfies every root requirement, every transitive dependency range and is
acyclic.  It raises ValueError on malformed input, a missing root, a missing
dependency or an unsatisfiable / necessarily cyclic graph.
"""
from .catalog import normalise

_INF = 1 << 62


def resolve(catalog, requirements):
    cat, roots = normalise(catalog, requirements)
    for name in roots:
        if name not in cat:
            raise ValueError('root %r missing from catalog' % (name,))
    solution = _search(cat, {}, roots)
    if solution is None:
        raise ValueError('unsatisfiable dependency graph')
    _verify(cat, solution, roots)
    return solution


def _deps(cat, name, version):
    """Return list of (depname, (lo, hi)) for a chosen package version.

    `normalise` stores each version as (digest, tuple(sorted(dep_items))), so
    the dependency mapping itself is `cat[name][version][1]` (a tuple of
    (depname, (lo, hi)) pairs).
    """
    return cat[name][version][1]


def _bounds(cat, bound, roots, name):
    """Tightest (lo, hi) for `name`: roots intersected with every bound requirer."""
    lo, hi = roots.get(name, (0, _INF))
    for requirer, version in bound.items():
        for dep, (dlo, dhi) in _deps(cat, requirer, version):
            if dep == name:
                if dlo > lo:
                    lo = dlo
                if dhi < hi:
                    hi = dhi
    return lo, hi


def _assigned_ok(cat, bound, roots):
    for name, version in bound.items():
        lo, hi = _bounds(cat, bound, roots, name)
        if not (lo <= version < hi):
            return False
    return True


def _edges(cat, name, version):
    return [dep for dep, _ in _deps(cat, name, version)]


def _reachable(cat, bound):
    """All names reachable (via assigned versions) from the roots/bound."""
    seen = set(bound)
    stack = list(bound)
    while stack:
        node = stack.pop()
        for dep in _edges(cat, node, bound[node]):
            if dep not in seen:
                seen.add(dep)
                if dep in bound:
                    stack.append(dep)
    return seen


def _open_needs(cat, bound, roots):
    """Names still to decide: roots plus every dependency reachable from them."""
    need = set(name for name in roots if name not in bound)
    seen = set(bound) | need
    stack = list(need) + [n for n in bound]
    while stack:
        node = stack.pop()
        version = bound.get(node)
        if version is None:
            # Not yet decided: look through every candidate version for edges.
            for candidate in cat.get(node, ()):
                for dep in _edges(cat, node, candidate):
                    if dep not in seen:
                        seen.add(dep)
                        stack.append(dep)
            continue
        for dep in _edges(cat, node, version):
            if dep not in seen:
                seen.add(dep)
                stack.append(dep)
    return set(name for name in seen if name not in bound)


def _deps_satisfiable(cat, bound, name, version):
    """Every dependency of name=version exists and fits current/possible bounds."""
    for dep, (lo, hi) in _deps(cat, name, version):
        if dep not in cat:
            return False
        if dep in bound and not (lo <= bound[dep] < hi):
            return False
    return True


def _acyclic(cat, solution):
    """True if the fully assigned solution graph has no cycle."""
    WHITE, GREY, BLACK = 0, 1, 2
    color = {name: WHITE for name in solution}
    for start in sorted(solution):
        if color[start] != WHITE:
            continue
        stack = [(start, iter(_edges(cat, start, solution[start])))]
        color[start] = GREY
        while stack:
            node, it = stack[-1]
            advanced = False
            for dep in it:
                if dep not in solution:
                    continue
                if color[dep] == GREY:
                    return False
                if color[dep] == WHITE:
                    color[dep] = GREY
                    stack.append((dep, iter(_edges(cat, dep, solution[dep]))))
                    advanced = True
                    break
            if not advanced:
                color[node] = BLACK
                stack.pop()
    return True


def _search(cat, bound, roots):
    if not _assigned_ok(cat, bound, roots):
        return None
    for name, version in bound.items():
        if not _deps_satisfiable(cat, bound, name, version):
            return None
    pending = _open_needs(cat, bound, roots)
    if not pending:
        if _acyclic(cat, bound):
            return dict(bound)
        return None
    name = min(pending)
    lo, hi = _bounds(cat, bound, roots, name)
    candidates = sorted((v for v in cat[name] if lo <= v < hi), reverse=True)
    for version in candidates:
        new = dict(bound)
        new[name] = version
        if not _deps_satisfiable(cat, new, name, version):
            continue
        found = _search(cat, new, roots)
        if found is not None:
            return found
    return None


def _verify(cat, solution, roots):
    for name in sorted(roots):
        if name not in solution:
            raise ValueError('root %r dropped' % (name,))
        lo, hi = roots[name]
        if not (lo <= solution[name] < hi):
            raise ValueError('root %r unsatisfied' % (name,))
    for name, version in solution.items():
        for dep, (lo, hi) in _deps(cat, name, version):
            if dep not in solution:
                raise ValueError('missing dependency %r' % (dep,))
            if not (lo <= solution[dep] < hi):
                raise ValueError('dependency %r unsatisfied' % (dep,))
    if not _acyclic(cat, solution):
        raise ValueError('cyclic dependency graph')
