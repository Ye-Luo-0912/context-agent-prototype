"""Catalog normalisation for the dependency resolver (SPEC.md, version 2)."""
from .util import NAME_RE

_UNKNOWN_VERSION_KEYS = {'version', 'sha256', 'deps'}


def _range(req):
    """Normalise a {min,max} requirement into an inclusive-exclusive (lo, hi)."""
    if not isinstance(req, dict):
        raise ValueError('requirement must be a mapping: %r' % (req,))
    extra = set(req) - {'min', 'max'}
    if extra:
        raise ValueError('unknown requirement keys: %r' % (sorted(extra),))
    lo, hi = req.get('min'), req.get('max')
    for bound in (lo, hi):
        if isinstance(bound, bool) or not isinstance(bound, int):
            raise ValueError('bounds must be integers: %r' % (req,))
    if lo >= hi:
        raise ValueError('empty range: %r' % (req,))
    return (lo, hi)


def normalise(catalog, requirements):
    """Return ({name: {version: (digest, ((dep,(lo,hi)),...))}}, {name:(lo,hi)}).

    Raises ValueError on any malformed catalog, requirement or missing root.
    """
    if not isinstance(catalog, dict):
        raise ValueError('catalog must be a mapping')
    if not isinstance(requirements, dict):
        raise ValueError('requirements must be a mapping')

    cat = {}
    for name, entries in catalog.items():
        if not isinstance(name, str) or not NAME_RE.match(name):
            raise ValueError('illegal package name: %r' % (name,))
        if not isinstance(entries, list) or not entries:
            raise ValueError('package %r must list versions' % (name,))
        versions = {}
        for entry in entries:
            if not isinstance(entry, dict):
                raise ValueError('entry for %r must be a mapping' % (name,))
            extra = set(entry) - _UNKNOWN_VERSION_KEYS
            if extra:
                raise ValueError('unknown entry keys for %r: %r' % (name, sorted(extra)))
            version = entry.get('version')
            if isinstance(version, bool) or not isinstance(version, int):
                raise ValueError('bad version for %r: %r' % (name, version))
            if version < 1:
                raise ValueError('non-positive version for %r: %r' % (name, version))
            digest = entry.get('sha256')
            if not isinstance(digest, str) or len(digest) != 64:
                raise ValueError('bad sha256 for %r: %r' % (name, digest))
            try:
                int(digest, 16)
            except ValueError:
                raise ValueError('bad sha256 for %r: %r' % (name, digest))
            deps = entry.get('deps')
            if not isinstance(deps, dict):
                raise ValueError('bad deps for %r' % (name,))
            parsed = {}
            for dname, dreq in deps.items():
                if not isinstance(dname, str) or not NAME_RE.match(dname):
                    raise ValueError('illegal dependency name: %r' % (dname,))
                parsed[dname] = _range(dreq)
            canon = (digest, tuple(sorted(parsed.items())))
            if version in versions:
                if versions[version] != canon:
                    raise ValueError('duplicate version %d/%r inconsistent' % (version, name))
                continue
            versions[version] = canon
        cat[name] = versions

    roots = {}
    for name, req in requirements.items():
        if not isinstance(name, str) or not NAME_RE.match(name):
            raise ValueError('illegal requirement name: %r' % (name,))
        roots[name] = _range(req)
    return cat, roots
