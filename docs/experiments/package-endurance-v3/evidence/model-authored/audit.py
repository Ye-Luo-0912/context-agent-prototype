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
import sys

DIGEST_RE = re.compile(r'^[0-9a-f]{64}$')
NAME_RE = re.compile(r'^[a-z][a-z0-9-]{0,63}$')
MANIFEST_KEYS = frozenset(('tenant', 'environment', 'packages'))
RECEIPT_KEYS = frozenset(('tenant', 'environment', 'key', 'generation', 'manifest_sha256'))


class AuditError(Exception):
    """An unrecoverable structural problem with the repository root."""


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
        return json.loads(text)
    except ValueError as exc:
        raise AuditError('invalid JSON in %s: %s' % (path, exc))


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
    path = os.path.join(os.fspath(root), 'repo.sqlite')
    if not os.path.isfile(path):
        raise AuditError('missing repo.sqlite under root')
    for sidecar in ('-wal', '-shm', '-journal'):
        if os.path.exists(path + sidecar):
            raise AuditError(
                'needs quiescent snapshot: repo.sqlite%s exists, so the snapshot '
                'may hold uncommitted WAL/rollback content; refusing to audit a '
                'possibly inconsistent read-only view' % sidecar)
    before = _stat_key(path)
    uri = 'file:%s?mode=ro&immutable=1' % path.replace('\\', '/')
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


def audit_manifest(root, digest, where, errors):
    """Verify the manifest file for ``digest`` and every blob it references.

    Returns the number of verified blobs, or None when the manifest itself is
    missing, corrupt or malformed (blobs are then not attributable).
    """
    if not isinstance(digest, str) or not DIGEST_RE.fullmatch(digest):
        errors.append('%s: unsafe manifest digest name %r' % (where, digest))
        return None
    path = os.path.join(os.fspath(root), 'manifests', '%s.json' % digest)
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
        validated.append(blob)

    verified = 0
    for blob in validated:
        obj_path = os.path.join(os.fspath(root), 'objects', blob)
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


def audit_pointer(root, receipt, where, errors):
    """Verify deployments/<t>/<e>/current.json matches its OWN durable receipt.

    A repository may hold many historical receipts for the same
    tenant/environment (e.g. generations 1 and 2); the pointer names only the
    current one, so it is matched against the receipt with the same identity,
    never against every historical receipt.
    """
    tenant, environment = receipt['tenant'], receipt['environment']
    try:
        check_name(tenant, 'tenant')
        check_name(environment, 'environment')
    except AuditError as exc:
        errors.append('%s: %s' % (where, exc))
        return
    path = os.path.join(os.fspath(root), 'deployments', tenant, environment, 'current.json')
    if not os.path.isfile(path):
        return  # absent pointer is not a mismatch; staleness is tolerated
    try:
        pointer = load_json_file(path)
    except AuditError as exc:
        errors.append('%s: %s' % (where, exc))
        return
    if not validate_receipt_shape(pointer, where + ' pointer', errors):
        return
    if pointer != receipt:
        errors.append('%s: pointer %s/%s does not match durable receipt'
                      % (where, tenant, environment))


def _pointer_path(root, tenant, environment):
    return os.path.join(os.fspath(root), 'deployments', tenant, environment, 'current.json')


def audit_pointers(root, receipts, errors):
    """Audit each current.json pointer against the durable receipts.

    Every readable pointer is matched against the receipt sharing its
    identity (tenant/environment/key). A pointer whose named receipt does not
    exist in the committed database is a missing DB receipt, and two durable
    receipts sharing one identity (so the pointer cannot name a unique current
    generation) is also flagged.
    """
    by_identity = {}
    for receipt in receipts:
        by_identity.setdefault(receipt_identity(receipt), []).append(receipt)

    base = os.path.join(os.fspath(root), 'deployments')
    if not os.path.isdir(base):
        return
    for tenant in sorted(os.listdir(base)):
        tdir = os.path.join(base, tenant)
        if not os.path.isdir(tdir) or not NAME_RE.fullmatch(tenant):
            continue
        for environment in sorted(os.listdir(tdir)):
            edir = os.path.join(tdir, environment)
            if not os.path.isdir(edir) or not NAME_RE.fullmatch(environment):
                continue
            pointer_path = os.path.join(edir, 'current.json')
            if not os.path.isfile(pointer_path):
                continue
            where = 'pointer %s/%s' % (tenant, environment)
            try:
                pointer = load_json_file(pointer_path)
            except AuditError as exc:
                errors.append('%s: %s' % (where, exc))
                continue
            if not validate_receipt_shape(pointer, where, errors):
                continue
            if pointer['tenant'] != tenant or pointer['environment'] != environment:
                errors.append('%s: names tenant/environment %s/%s'
                              % (where, pointer['tenant'], pointer['environment']))
                continue
            matches = by_identity.get(receipt_identity(pointer))
            if not matches:
                errors.append('%s: missing DB receipt for %s/%s/%s'
                              % (where, pointer['tenant'], pointer['environment'],
                                 pointer['key']))
                continue
            if len(matches) > 1:
                errors.append('%s: ambiguous durable receipts for %s/%s/%s'
                              % (where, pointer['tenant'], pointer['environment'],
                                 pointer['key']))
                continue
            if matches[0] != pointer:
                errors.append('%s: does not match durable receipt' % where)


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
            audit_manifest(root, obj['manifest_sha256'], where, errors)
            receipts.append(obj)

        # Each current pointer names its own identity's durable receipt; a
        # pointer with no committed receipt is a missing DB receipt.
        audit_pointers(root, receipts, errors)
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
