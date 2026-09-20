"""Read-only repository auditor for the v2 package repository (SPEC.md).

CLI: ``python -m app.audit --root PATH``

Enumerates every committed receipt in ``repo.sqlite`` (opened read-only without
ever creating or modifying any file), verifies the canonical manifest bytes
hash to ``manifest_sha256``, that every referenced object/blob hashes to its
recorded SHA256, and that each ``deployments/<tenant>/<environment>/current.json``
pointer names a durable, internally consistent receipt.

The auditor NEVER writes, repairs or executes anything: no payloads run, no
network is touched, and the SQLite database (plus its WAL/journal sidecars) is
opened only when the snapshot is quiescent. Unsafe digest names, path escapes
and malformed JSON shapes are reported as errors rather than followed.

Output is a single JSON object ``{"ok": bool, "errors": [str, ...],
"receipts": int}`` and the process exits 0 when sound, 1 on any corruption.
"""
import argparse
import hashlib
import json
import os
import re
import sqlite3
import stat
import sys
from pathlib import Path

DIGEST_RE = re.compile(r'^[0-9a-f]{64}$')
NAME_RE = re.compile(r'^[a-z][a-z0-9-]{0,63}$')
MANIFEST_KEYS = frozenset(('tenant', 'environment', 'packages'))
RECEIPT_KEYS = frozenset(('tenant', 'environment', 'key', 'generation', 'manifest_sha256'))


class AuditError(Exception):
    """An unrecoverable structural problem with the repository root."""


def safe_path(root, *parts):
    """Reject links/reparse points at every component of a quiescent snapshot.

    Digest/name validation alone does not prevent an objects directory junction
    or a named file symlink from redirecting reads outside the supplied root.
    The root must stay quiescent throughout inspection; this is not an openat
    sandbox against a process actively replacing files during the audit.
    """
    base = os.path.abspath(os.fspath(root))
    path = os.path.abspath(os.path.join(base, *parts))
    if os.path.commonpath((base, path)) != base:
        raise AuditError('path escapes snapshot root')
    relative = os.path.relpath(path, base)
    candidates = [base]
    if relative != '.':
        for part in relative.split(os.sep):
            candidates.append(os.path.join(candidates[-1], part))
    for candidate in candidates:
        try:
            info = os.lstat(candidate)
        except FileNotFoundError:
            continue
        except OSError as exc:
            raise AuditError('cannot inspect path %s: %s' % (candidate, exc)) from exc
        if stat.S_ISLNK(info.st_mode) or getattr(info, 'st_file_attributes', 0) & getattr(stat, 'FILE_ATTRIBUTE_REPARSE_POINT', 0x400):
            raise AuditError('unsafe link or reparse point: %s' % candidate)
    return path


def check_name(value, label):
    if not isinstance(value, str) or not NAME_RE.fullmatch(value):
        raise AuditError('illegal %s %r' % (label, value))
    return value


def sha256_file(path):
    """Hash a file's real bytes; a read error means the object is missing."""
    h = hashlib.sha256()
    with open(os.fspath(path), 'rb') as fh:
        for chunk in iter(lambda: fh.read(65536), b''):
            h.update(chunk)
    return h.hexdigest()


def canonical_bytes(obj):
    """Canonical encoding used for hashed content (SPEC.md)."""
    text = json.dumps(obj, sort_keys=True, separators=(',', ':'),
                      allow_nan=False, ensure_ascii=False)
    return text.encode('utf-8')


def load_json_file(path):
    """Parse a JSON file strictly; raises AuditError on any problem."""
    try:
        with open(os.fspath(path), 'rb') as fh:
            raw = fh.read()
    except OSError as exc:
        raise AuditError('unreadable file %s: %s' % (path, exc.strerror or exc))
    try:
        text = raw.decode('utf-8')
    except UnicodeDecodeError:
        raise AuditError('non-UTF-8 content in %s' % path)
    try:
        return strict_json(text)
    except ValueError as exc:
        raise AuditError('invalid JSON in %s: %s' % (path, exc))


def strict_json(text):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError('duplicate JSON key %r' % key)
            result[key] = value
        return result
    def constant(value):
        raise ValueError('non-finite JSON value %s' % value)
    return json.loads(text, object_pairs_hook=pairs, parse_constant=constant)


def open_readonly(root):
    """Open repo.sqlite read-only without creating/modifying any file.

    This auditor is only for QUIESCENT snapshot roots. A quiescent SQLite
    database has no committed data outside the main file, so:

    * any existing ``-wal``/``-shm``/``-journal`` sidecar means the snapshot may
      hold committed data we cannot safely read (and reading it via the SQLite
      API would create/consume sidecars in an otherwise untouched root) -> the
      root is refused with an honest needs-quiescent error; and
    * the main file is opened through SQLite's URI ``mode=ro`` with
      ``immutable=1`` so no ``-wal``/``-shm`` is ever created and all
      record/schema metadata is parsed by SQLite itself (no custom parser).

    ``immutable=1`` is sound here precisely because the sidecar check above
    guarantees no committed WAL/rollback content exists outside the main file.
    """
    path = safe_path(root, 'repo.sqlite')
    if not os.path.isfile(path):
        raise AuditError('missing repo.sqlite under root')
    for sidecar in ('-wal', '-shm', '-journal'):
        if os.path.lexists(path + sidecar):
            raise AuditError(
                'needs quiescent snapshot: repo.sqlite%s exists, so the snapshot '
                'may hold uncommitted WAL/rollback content; refusing to audit a '
                'possibly inconsistent read-only view' % sidecar)
    before = _stat_key(path)
    uri = Path(path).as_uri() + '?mode=ro&immutable=1'
    try:
        conn = sqlite3.connect(
            uri, uri=True,
            check_same_thread=False,
            isolation_level=None)
    except sqlite3.Error as exc:
        raise AuditError('cannot open repo.sqlite read-only: %s' % (exc,))
    # Force header validation now; connect() alone is lazy.
    try:
        conn.execute('PRAGMA schema_version').fetchall()
    except sqlite3.Error as exc:
        conn.close()
        raise AuditError('cannot read repo.sqlite read-only: %s' % (exc,))
    after = _stat_key(path)
    if before != after:
        conn.close()
        raise AuditError('repo.sqlite changed during open; refusing to audit')
    conn.row_factory = sqlite3.Row
    return conn


def _read_bytes(path):
    with open(path, 'rb') as fh:
        return fh.read()


def _stat_key(path):
    st = os.stat(path)
    return (st.st_size, st.st_mtime_ns)


def enumerate_receipts(conn, errors):
    """Read every committed receipt row; malformed rows become errors."""
    try:
        columns = [row[1] for row in conn.execute('PRAGMA table_info(receipts)')]
    except sqlite3.Error as exc:
        errors.append('cannot inspect receipts table: %s' % exc)
        return []
    required = ('tenant', 'environment', 'key', 'generation', 'manifest_sha256')
    if not all(c in columns for c in required):
        errors.append('receipts table missing required columns: %s'
                      % (', '.join(required),))
        return []
    try:
        rows = conn.execute(
            'SELECT tenant, environment, key, generation, manifest_sha256'
            ' FROM receipts').fetchall()
    except sqlite3.Error as exc:
        errors.append('cannot read receipts table: %s' % exc)
        return []
    return rows


def receipt_object(row):
    """Build the canonical receipt object for a stored row (or None if invalid)."""
    try:
        return {
            'tenant': row['tenant'],
            'environment': row['environment'],
            'key': row['key'],
            'generation': row['generation'],
            'manifest_sha256': row['manifest_sha256'],
        }
    except (IndexError, KeyError):
        return None


def validate_receipt_shape(obj, where, errors):
    """Reject malformed receipt objects; return True when structurally sound."""
    if not isinstance(obj, dict) or set(obj) != RECEIPT_KEYS:
        errors.append('%s: receipt is not exactly %s' % (where, sorted(RECEIPT_KEYS)))
        return False
    ok = True
    if isinstance(obj['generation'], bool) or not isinstance(obj['generation'], int) \
            or obj['generation'] < 1:
        errors.append('%s: generation must be a positive integer' % where)
        ok = False
    for field in ('tenant', 'environment', 'key'):
        if not isinstance(obj[field], str) or not NAME_RE.fullmatch(obj[field]):
            errors.append('%s: illegal %s %r' % (where, field, obj[field]))
            ok = False
    if not isinstance(obj['manifest_sha256'], str) \
            or not DIGEST_RE.fullmatch(obj['manifest_sha256']):
        errors.append('%s: unsafe manifest digest name %r'
                      % (where, obj['manifest_sha256']))
        ok = False
    return ok


def audit_manifest(root, digest, where, errors, expected_scope):
    """Verify the manifest file for ``digest`` and every blob it references.

    Returns the number of verified blobs, or None when the manifest itself is
    missing, corrupt or malformed (blobs are then not attributable).
    """
    if not isinstance(digest, str) or not DIGEST_RE.fullmatch(digest):
        errors.append('%s: unsafe manifest digest name %r' % (where, digest))
        return None
    path = safe_path(root, 'manifests', '%s.json' % digest)
    if not os.path.isfile(path):
        errors.append('%s: missing manifest %s' % (where, digest))
        return None
    try:
        if sha256_file(path) != digest:
            errors.append('%s: corrupt manifest %s (SHA256 mismatch)' % (where, digest))
            return None
    except OSError as exc:
        errors.append('%s: unreadable manifest %s: %s' % (where, digest, exc.strerror or exc))
        return None

    try:
        manifest = load_json_file(path)
    except AuditError as exc:
        errors.append('%s: %s' % (where, exc))
        return None
    if not isinstance(manifest, dict) or set(manifest) != MANIFEST_KEYS:
        errors.append('%s: manifest %s has malformed shape' % (where, digest))
        return None
    for field in ('tenant', 'environment'):
        if not isinstance(manifest[field], str) or not NAME_RE.fullmatch(manifest[field]):
            errors.append('%s: manifest has illegal %s' % (where, field))
            return None
    if (manifest['tenant'], manifest['environment']) != expected_scope:
        errors.append('%s: manifest scope differs from receipt scope' % where)
        return None
    try:
        if canonical_bytes(manifest) != _read_bytes(path):
            errors.append('%s: manifest %s is not canonically encoded'
                          % (where, digest))
            return None
    except OSError as exc:
        errors.append('%s: unreadable manifest %s: %s' % (where, digest, exc.strerror or exc))
        return None
    packages = manifest['packages']
    if not isinstance(packages, list):
        errors.append('%s: manifest %s has malformed packages (not a list)'
                      % (where, digest))
        return None

    seen = set()
    validated = []
    names = []
    for index, package in enumerate(packages):
        label = '%s: manifest %s package[%d]' % (where, digest, index)
        if not isinstance(package, dict) or set(package) != {'name', 'version', 'sha256'}:
            errors.append('%s has malformed shape' % label)
            continue
        name, version, blob = package['name'], package['version'], package['sha256']
        if not isinstance(name, str) or not NAME_RE.fullmatch(name):
            errors.append('%s: illegal package name %r' % (label, name))
            continue
        if isinstance(version, bool) or not isinstance(version, int) or version < 1:
            errors.append('%s: version must be a positive integer' % label)
            continue
        if not isinstance(blob, str) or not DIGEST_RE.fullmatch(blob):
            errors.append('%s: unsafe blob digest name %r' % (label, blob))
            continue
        if name in seen:
            errors.append('%s: duplicate package name %r' % (label, name))
            continue
        seen.add(name)
        names.append(name)
        validated.append(blob)

    if names != sorted(names):
        errors.append('%s: manifest packages are not sorted by name' % where)

    verified = 0
    for blob in validated:
        obj_path = safe_path(root, 'objects', blob)
        if not os.path.isfile(obj_path):
            errors.append('%s: missing blob object %s' % (where, blob))
            continue
        try:
            if sha256_file(obj_path) != blob:
                errors.append('%s: corrupt blob object %s (SHA256 mismatch)' % (where, blob))
                continue
        except OSError as exc:
            errors.append('%s: unreadable blob %s: %s' % (where, blob, exc.strerror or exc))
            continue
        verified += 1
    return verified


def receipt_identity(obj):
    """The identity a durable receipt and its current pointer must share."""
    return (obj['tenant'], obj['environment'], obj['key'])


def enumerate_current(conn, receipts, errors):
    """SQLite current is authoritative; historical membership is insufficient."""
    by_identity = {}
    for receipt in receipts:
        identity = receipt_identity(receipt)
        if identity in by_identity:
            errors.append('duplicate durable receipt identity %s/%s/%s' % identity)
        by_identity[identity] = receipt
    try:
        rows = conn.execute('SELECT tenant,environment,receipt_json FROM current').fetchall()
    except sqlite3.Error as exc:
        errors.append('cannot enumerate current authority: %s' % exc)
        return {}, by_identity
    current = {}
    for row in rows:
        where = 'SQLite current %s/%s' % (row['tenant'], row['environment'])
        try:
            check_name(row['tenant'], 'current tenant')
            check_name(row['environment'], 'current environment')
            obj = strict_json(row['receipt_json'])
        except (AuditError, ValueError, TypeError) as exc:
            errors.append('%s: malformed current receipt: %s' % (where, exc))
            continue
        if not validate_receipt_shape(obj, where, errors):
            continue
        scope = (row['tenant'], row['environment'])
        if (obj['tenant'], obj['environment']) != scope:
            errors.append('%s: receipt scope differs from current row' % where)
            continue
        if scope in current:
            errors.append('%s: duplicate current scope' % where)
        current[scope] = obj
        if by_identity.get(receipt_identity(obj)) != obj:
            errors.append('%s: current does not name an exact durable receipt' % where)
    return current, by_identity


def audit_pointers(root, receipts, conn, errors):
    current, by_identity = enumerate_current(conn, receipts, errors)
    observed = set()
    base = safe_path(root, 'deployments')
    if os.path.isdir(base):
        for tenant in sorted(os.listdir(base)):
            tdir = safe_path(root, 'deployments', tenant)
            if not os.path.isdir(tdir):
                continue
            if not NAME_RE.fullmatch(tenant):
                errors.append('illegal deployment tenant directory %r' % tenant)
                continue
            for environment in sorted(os.listdir(tdir)):
                edir = safe_path(root, 'deployments', tenant, environment)
                if not os.path.isdir(edir):
                    continue
                if not NAME_RE.fullmatch(environment):
                    errors.append('illegal deployment environment directory %r' % environment)
                    continue
                path = safe_path(root, 'deployments', tenant, environment, 'current.json')
                if not os.path.isfile(path):
                    continue
                scope = (tenant, environment)
                observed.add(scope)
                where = 'pointer %s/%s' % scope
                try:
                    pointer = load_json_file(path)
                except AuditError as exc:
                    errors.append('%s: %s' % (where, exc))
                    continue
                if not validate_receipt_shape(pointer, where, errors):
                    continue
                if (pointer['tenant'], pointer['environment']) != scope:
                    errors.append('%s: pointer scope differs from its path' % where)
                    continue
                if canonical_bytes(pointer) != _read_bytes(path):
                    errors.append('%s: pointer is not canonically encoded' % where)
                if by_identity.get(receipt_identity(pointer)) != pointer:
                    errors.append('%s: missing or mismatched durable receipt' % where)
                if current.get(scope) != pointer:
                    errors.append('%s: pointer differs from SQLite current authority' % where)
    for scope in current.keys() - observed:
        errors.append('missing pointer for SQLite current %s/%s' % scope)


def audit(root):
    """Audit a repository root; return {'ok', 'errors', 'receipts'}."""
    errors = []
    root = os.fspath(root)
    if not os.path.isdir(root):
        errors.append('root is not a directory: %s' % root)
        return {'ok': False, 'errors': errors, 'receipts': 0}

    try:
        conn = open_readonly(root)
    except AuditError as exc:
        errors.append(str(exc))
        return {'ok': False, 'errors': errors, 'receipts': 0}

    receipts = []
    try:
        rows = enumerate_receipts(conn, errors)
        for index, row in enumerate(rows):
            obj = receipt_object(row)
            where = 'receipt[%d]' % index
            if obj is None or not validate_receipt_shape(obj, where, errors):
                continue
            where = 'receipt %s/%s/%s' % (obj['tenant'], obj['environment'], obj['key'])
            audit_manifest(root, obj['manifest_sha256'], where, errors,
                           (obj['tenant'], obj['environment']))
            receipts.append(obj)

        # Each current pointer names its own identity's durable receipt; a
        # pointer with no committed receipt is a missing DB receipt.
        audit_pointers(root, receipts, conn, errors)
    except (AuditError, OSError, ValueError, TypeError) as exc:
        errors.append('snapshot audit incomplete: %s' % exc)
    finally:
        conn.close()

    return {'ok': not errors, 'errors': errors, 'receipts': len(receipts)}


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog='app.audit', description='Read-only repository auditor.')
    parser.add_argument('--root', required=True, help='repository root to audit')
    args = parser.parse_args(argv)
    result = audit(args.root)
    sys.stdout.write(json.dumps(result, sort_keys=True, separators=(',', ':'),
                                allow_nan=False) + '\n')
    return 0 if result['ok'] else 1


if __name__ == '__main__':
    sys.exit(main())
