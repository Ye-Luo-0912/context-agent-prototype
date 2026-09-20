"""Shared helpers for the v2 implementation (SPEC.md)."""
import json
import os
import re
import hashlib
from pathlib import Path

# Legal names match [a-z][a-z0-9-]{0,63}: lowercase start, then lowercase,
# digits or hyphens. No dots, underscores or leading digits.
NAME_RE = re.compile(r'^[a-z][a-z0-9-]{0,63}$')
SEMVER_RE = re.compile(r'^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$')


def check_name(value, label):
    """Validate an identifier used as a tenant/environment/key segment."""
    if not isinstance(value, str) or not NAME_RE.match(value):
        raise ValueError('illegal %s: %r' % (label, value))
    return value


def check_key(value, label='key'):
    return check_name(value, label)


def canonical_bytes(obj):
    """Canonical JSON bytes: sorted keys, compact, no NaN, non-ASCII preserved.

    No trailing newline is appended; callers that need a terminator add one
    explicitly.
    """
    text = json.dumps(obj, sort_keys=True, separators=(',', ':'),
                      allow_nan=False, ensure_ascii=False)
    return text.encode('utf-8')


def atomic_write(path, data):
    """Write bytes durably: temp file in same dir, fsync, atomic replace."""
    if isinstance(data, str):
        data = data.encode('utf-8')
    path = os.fspath(path)
    d = os.path.dirname(path) or '.'
    os.makedirs(d, exist_ok=True)
    tmp = '%s.tmp.%d' % (path, os.getpid())
    with open(tmp, 'wb') as fh:
        fh.write(data)
        fh.flush()
        os.fsync(fh.fileno())
    os.replace(tmp, path)
    try:
        dfd = os.open(d, os.O_RDONLY)
        try:
            os.fsync(dfd)
        finally:
            os.close(dfd)
    except OSError:
        pass


def atomic_write_json(path, obj):
    atomic_write(path, canonical_bytes(obj))


def read_json(path):
    with open(path, 'rb') as fh:
        return json.loads(fh.read().decode('utf-8'))


def sha256_bytes(data):
    if isinstance(data, str):
        data = data.encode('utf-8')
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    h = hashlib.sha256()
    with open(os.fspath(path), 'rb') as fh:
        for chunk in iter(lambda: fh.read(65536), b''):
            h.update(chunk)
    return h.hexdigest()


def parse_semver(text):
    if not isinstance(text, str):
        raise ValueError('bad version: %r' % (text,))
    m = SEMVER_RE.match(text)
    if not m:
        raise ValueError('bad version: %r' % (text,))
    return tuple(int(x) for x in m.groups())
