"""Transactional deploy implementation: journal, install, GC, migrate, publish.

Uses an on-disk JSON journal (durable across os._exit crashes) plus the SQLite
authority tables defined in app/state.py. Crash hooks call os._exit(71/72).
"""
import hashlib
import json
import os
import urllib.error
import urllib.request
from pathlib import Path

from app import state
from app.util import (atomic_write, atomic_write_json, canonical_bytes,
                      check_name, read_json, sha256_bytes, sha256_file)

CRASH_POINTER = 71
CRASH_MIGRATION = 72
LEGACY_TENANT = 'legacy'
LEGACY_ENVIRONMENT = 'default'


# --------------------------------------------------------------------------- #
# canonical objects & paths
# --------------------------------------------------------------------------- #
def manifest_obj(tenant, environment, packages):
    return {'tenant': tenant, 'environment': environment,
            'packages': [{'name': n, 'version': v, 'sha256': d} for n, (v, d) in packages]}


def receipt_obj(tenant, environment, key, generation, manifest_sha256):
    return {'tenant': tenant, 'environment': environment, 'key': key,
            'generation': generation, 'manifest_sha256': manifest_sha256}


def pointer_valid(pointer):
    if not isinstance(pointer, dict):
        return False
    if set(pointer) != {'tenant', 'environment', 'key', 'generation', 'manifest_sha256'}:
        return False
    if isinstance(pointer['generation'], bool) or not isinstance(pointer['generation'], int) or pointer['generation'] < 1:
        return False
    digest = pointer['manifest_sha256']
    if not isinstance(digest, str) or len(digest) != 64 or any(c not in '0123456789abcdef' for c in digest):
        return False
    for name in ('tenant', 'environment', 'key'):
        try:
            check_name(pointer[name], name)
        except ValueError:
            return False
    return True


def deployment_dir(root, tenant, environment):
    check_name(tenant, 'tenant')
    check_name(environment, 'environment')
    return Path(root) / 'deployments' / tenant / environment


def pointer_path(root, tenant, environment):
    return deployment_dir(root, tenant, environment) / 'current.json'


def manifest_path(root, digest):
    return Path(root) / 'manifests' / ('%s.json' % digest)


def journal_dir(root):
    return Path(root) / 'journal'


def journal_path(root, tenant, environment, key):
    check_name(tenant, 'tenant')
    check_name(environment, 'environment')
    check_name(key, 'key')
    return journal_dir(root) / ('%s__%s__%s.json' % (tenant, environment, key))


def read_pointer(root, tenant, environment):
    path = pointer_path(root, tenant, environment)
    if not os.path.exists(path):
        return None
    try:
        return read_json(path)
    except (OSError, ValueError):
        return None


def _crash(boundary):
    os._exit(CRASH_MIGRATION if boundary == 'migration_after_copy' else CRASH_POINTER)


def _request_json(requirements, expected_generation):
    return json.dumps({'requirements': requirements, 'expected_generation': expected_generation},
                      sort_keys=True, separators=(',', ':'), allow_nan=False)


# --------------------------------------------------------------------------- #
# verified blobs / objects / manifests
# --------------------------------------------------------------------------- #
def read_verified(path, expected):
    if not os.path.exists(path):
        return None
    try:
        data = Path(path).read_bytes()
    except OSError:
        return None
    if hashlib.sha256(data).hexdigest() != expected:
        return None
    return data


def resolve_sources(catalog_path, catalog, solution):
    src = Path(catalog_path).parent / 'blobs'
    out = {}
    for name in sorted(solution):
        version = solution[name]
        entry = next((e for e in catalog[name] if e['version'] == version), None)
        if entry is None:
            raise ValueError('resolved version vanished: %s==%r' % (name, version))
        digest = entry['sha256']
        if digest in out:
            continue
        data = read_verified(src / digest, digest)
        if data is None:
            raise ValueError('missing/corrupt source blob %s for %s==%d' % (digest, name, version))
        out[digest] = data
    return out


def ensure_object(root, digest, data):
    dest = Path(root) / 'objects' / digest
    if os.path.exists(dest) and sha256_file(dest) == digest:
        return
    atomic_write(dest, data)
    if sha256_file(dest) != digest:
        raise ValueError('object store corruption at %s' % digest)


def prepare(root, catalog_path, catalog, solution):
    """Verify blobs, harden object store, return packages [(name,(version,digest))]."""
    blobs = resolve_sources(catalog_path, catalog, solution)
    packages = []
    for name in sorted(solution):
        version = solution[name]
        digest = next(e for e in catalog[name] if e['version'] == version)['sha256']
        ensure_object(root, digest, blobs[digest])
        packages.append((name, (version, digest)))
    return packages


def write_manifest(root, tenant, environment, packages):
    raw = canonical_bytes(manifest_obj(tenant, environment, packages))
    digest = sha256_bytes(raw)
    path = manifest_path(root, digest)
    if os.path.exists(path):
        if Path(path).read_bytes() != raw:
            raise ValueError('manifest collision for %s' % digest)
    else:
        atomic_write(path, raw)
    return digest


# --------------------------------------------------------------------------- #
# on-disk journal
# --------------------------------------------------------------------------- #
def journal_write(root, tenant, environment, key, record):
    journal_dir(root).mkdir(parents=True, exist_ok=True)
    atomic_write(journal_path(root, tenant, environment, key), canonical_bytes(record))


def journal_clear(root, tenant, environment, key):
    try:
        os.unlink(journal_path(root, tenant, environment, key))
    except OSError:
        pass


def _disk_journal_records(root):
    records = []
    d = journal_dir(root)
    if not d.is_dir():
        return records
    for entry in sorted(d.iterdir()):
        if not entry.name.endswith('.json'):
            continue
        try:
            records.append(read_json(entry))
        except (OSError, ValueError):
            continue
    return records


# --------------------------------------------------------------------------- #
# install transaction
# --------------------------------------------------------------------------- #
def install(repo, tenant, environment, key, requirements, expected_generation, crash_at):
    """Serialize filesystem mutation with the same SQLite writer lock as GC.

    The disk journal survives a rolled-back SQL transaction after process death.
    There is no unlocked gap between selecting a generation and publishing it.
    """
    conn, root = repo.conn, repo.root
    request_json = _request_json(requirements, expected_generation)
    solution = repo._resolve(requirements)  # Validation is mutation-free.
    _begin_reconciled(repo)
    try:
        existing = state.read_receipt(conn, tenant, environment, key)
        if existing is not None:
            if existing['request_json'] != request_json:
                raise ValueError('identity %s/%s/%s bound to different content'
                                 % (tenant, environment, key))
            receipt = receipt_obj(tenant, environment, key, existing['generation'],
                                  existing['manifest_sha256'])
            # Only SQLite current may heal the projection, never an old receipt.
            _heal_current_locked(repo, tenant, environment)
            state.commit(conn)
            return receipt

        current = state.read_current(conn, tenant, environment)
        current_obj = json.loads(current['receipt_json']) if current else None
        current_gen = current_obj['generation'] if current_obj else 0
        if expected_generation is not None and expected_generation != current_gen:
            raise ValueError('expected_generation %r != current %r'
                             % (expected_generation, current_gen))
        packages = prepare(root, repo.catalog_path, repo.catalog, solution)
        digest = write_manifest(root, tenant, environment, packages)
        receipt = receipt_obj(tenant, environment, key, current_gen + 1, digest)
        record = {'tenant': tenant, 'environment': environment, 'key': key,
                  'generation': current_gen + 1, 'manifest_sha256': digest,
                  'request_json': request_json, 'old_receipt': current_obj}
        journal_write(root, tenant, environment, key, record)
        if crash_at == 'before_pointer':
            _crash('before_pointer')
        atomic_write_json(pointer_path(root, tenant, environment), receipt)
        if crash_at == 'after_pointer':
            _crash('after_pointer')
        state.put_current(conn, tenant, environment, receipt)
        state.insert_receipt(conn, tenant, environment, key, current_gen + 1,
                             digest, request_json)
        state.commit(conn)
    except BaseException:
        state.rollback(conn)
        raise
    # Removal also needs the writer lock: another process may be enumerating
    # recovery evidence after our commit and must not lose files mid-read.
    state.begin(conn)
    try:
        journal_clear(root, tenant, environment, key)
        state.commit(conn)
    except BaseException:
        state.rollback(conn)
        raise
    return receipt


def _heal_current_locked(repo, tenant, environment):
    current = state.read_current(repo.conn, tenant, environment)
    path = pointer_path(repo.root, tenant, environment)
    if current is None:
        if path.exists():
            path.unlink()
        return
    receipt = json.loads(current['receipt_json'])
    if not pointer_valid(receipt) or not _manifest_intact(repo.root, receipt['manifest_sha256']):
        raise ValueError('unreadable committed current; recovery required')
    if read_pointer(repo.root, tenant, environment) != receipt:
        atomic_write_json(path, receipt)


def _checked_disk_journals(root):
    directory = journal_dir(root)
    if not directory.exists():
        return []
    records = []
    for path in sorted(directory.glob('*.json')):
        try:
            record = read_json(path)
            receipt = {field: record[field] for field in
                       ('tenant', 'environment', 'key', 'generation', 'manifest_sha256')}
            if not pointer_valid(receipt):
                raise ValueError('invalid receipt')
            if path != journal_path(root, receipt['tenant'], receipt['environment'], receipt['key']):
                raise ValueError('journal identity/path mismatch')
            if not isinstance(record['request_json'], str):
                raise ValueError('missing original request')
            request = json.loads(record['request_json'])
            if set(request) != {'requirements', 'expected_generation'}:
                raise ValueError('invalid original request')
            old = record.get('old_receipt')
            if old is not None and (not pointer_valid(old)
                    or (old['tenant'], old['environment']) != (receipt['tenant'], receipt['environment'])
                    or old['generation'] + 1 != receipt['generation']):
                raise ValueError('invalid predecessor')
            if old is None and receipt['generation'] != 1:
                raise ValueError('missing predecessor')
        except (OSError, ValueError, TypeError, KeyError) as exc:
            raise ValueError('unreadable recovery journal %s' % path.name) from exc
        records.append((record, receipt))
    return records


def _reconcile_locked(repo):
    """Caller owns BEGIN IMMEDIATE; malformed evidence fences all mutation."""
    healed = 0
    for record, receipt in _checked_disk_journals(repo.root):
        tenant, environment, key = (receipt[x] for x in ('tenant', 'environment', 'key'))
        existing = state.read_receipt(repo.conn, tenant, environment, key)
        current = state.read_current(repo.conn, tenant, environment)
        current_obj = json.loads(current['receipt_json']) if current else None
        if existing is not None:
            if (existing['generation'], existing['manifest_sha256'], existing['request_json']) != (
                    receipt['generation'], receipt['manifest_sha256'], record['request_json']):
                raise ValueError('journal conflicts with committed identity')
            _heal_current_locked(repo, tenant, environment)
        elif current_obj != record.get('old_receipt'):
            # A journal may never replace a later SQLite generation.
            raise ValueError('unsettled journal predecessor differs from current')
        elif read_pointer(repo.root, tenant, environment) == receipt:
            if not _manifest_intact(repo.root, receipt['manifest_sha256']):
                raise ValueError('pending pointer has incomplete content')
            state.insert_receipt(repo.conn, tenant, environment, key, receipt['generation'],
                                 receipt['manifest_sha256'], record['request_json'])
            state.put_current(repo.conn, tenant, environment, receipt)
            healed += 1
        else:
            _heal_current_locked(repo, tenant, environment)
        # Clear only after commit in reconcile/install: deleting now could lose
        # after_pointer evidence if this transaction itself is interrupted.
    return healed


def _clear_settled_journals_locked(repo):
    """Called after commit while a NEW writer lock excludes other installs.

    The record is removable when its receipt is committed, or when the pointer
    and DB both retain the predecessor (a before_pointer cancellation).
    """
    for record, receipt in _checked_disk_journals(repo.root):
        tenant, environment, key = (receipt[x] for x in ('tenant', 'environment', 'key'))
        existing = state.read_receipt(repo.conn, tenant, environment, key)
        current = state.read_current(repo.conn, tenant, environment)
        current_obj = json.loads(current['receipt_json']) if current else None
        if existing is not None and (
                existing['generation'], existing['manifest_sha256'], existing['request_json']) != (
                    receipt['generation'], receipt['manifest_sha256'], record['request_json']):
            raise ValueError('journal conflicts with committed identity')
        if existing is not None or (current_obj == record.get('old_receipt')
                                    and read_pointer(repo.root, tenant, environment) == current_obj):
            if existing is not None:
                _heal_current_locked(repo, tenant, environment)
            journal_clear(repo.root, tenant, environment, key)


def _begin_reconciled(repo):
    """Return holding the writer lock only after committed recovery is settled."""
    healed = 0
    for _ in range(32):
        state.begin(repo.conn)
        try:
            _clear_settled_journals_locked(repo)
            if not _checked_disk_journals(repo.root):
                return healed
            healed += _reconcile_locked(repo)
            state.commit(repo.conn)
        except BaseException:
            state.rollback(repo.conn)
            raise
    raise ValueError('recovery repeatedly interrupted by new writers')


def reconcile(repo):
    healed = _begin_reconciled(repo)
    state.commit(repo.conn)
    return healed


def _manifest_intact(root, digest):
    if not isinstance(digest, str) or len(digest) != 64:
        return False
    data = read_verified(manifest_path(root, digest), digest)
    if data is None:
        return False
    try:
        manifest = json.loads(data)
        return all(read_verified(Path(root) / 'objects' / item['sha256'], item['sha256'])
                   is not None for item in manifest['packages'])
    except (ValueError, KeyError, TypeError):
        return False


# --------------------------------------------------------------------------- #
# GC: one SQLite writer transaction spans root enumeration and deletion.
# --------------------------------------------------------------------------- #
def gc(repo):
    root, conn = repo.root, repo.conn
    try:
        _begin_reconciled(repo)
        referenced = {row['manifest_sha256'] for row in state.all_receipts(conn)}
        for record, _ in _checked_disk_journals(root):
            referenced.add(record['manifest_sha256'])
        for row in state.journals(conn):
            referenced.add(json.loads(row['new_receipt'])['manifest_sha256'])
        live_blobs = set()
        for digest in referenced:
            data = read_verified(manifest_path(root, digest), digest)
            if data is None:
                raise ValueError('unreadable GC root')
            for package in json.loads(data)['packages']:
                live_blobs.add(package['sha256'])
        # Complete collection precedes the first deletion.
        deleted_objects = deleted_manifests = 0
        for entry in (Path(root) / 'objects').iterdir():
            if entry.is_file() and entry.name not in live_blobs:
                entry.unlink()
                deleted_objects += 1
        for entry in (Path(root) / 'manifests').iterdir():
            if entry.is_file() and entry.name.endswith('.json') and entry.stem not in referenced:
                entry.unlink()
                deleted_manifests += 1
        state.commit(conn)
        return {'deleted_objects': deleted_objects, 'deleted_manifests': deleted_manifests,
                'deferred': False}
    except (OSError, ValueError, KeyError, TypeError):
        state.rollback(conn)
        return {'deleted_objects': 0, 'deleted_manifests': 0, 'deferred': True}
    except BaseException:
        state.rollback(conn)
        raise


# --------------------------------------------------------------------------- #
# migration from v1
# --------------------------------------------------------------------------- #
def migrate(repo, crash_at=None):
    conn = repo.conn
    version = conn.execute('PRAGMA user_version').fetchone()[0]
    if version > 2:
        raise ValueError('unsupported future schema version %r' % (version,))
    if version == 0:
        conn.execute('PRAGMA user_version=2')
        return {'from': 0, 'to': 2, 'migrated': 0}
    if version == 2:
        return {'from': 2, 'to': 2, 'migrated': 0}
    # version == 1
    cols = [r[1] for r in conn.execute('PRAGMA table_info(receipts)').fetchall()]
    if 'tenant' in cols:
        conn.execute('PRAGMA user_version=2')
        return {'from': 1, 'to': 2, 'migrated': 0}
    conn.executescript("""
        CREATE TABLE IF NOT EXISTS receipts_v2(
            tenant TEXT NOT NULL, environment TEXT NOT NULL, key TEXT NOT NULL,
            generation INTEGER NOT NULL, manifest_sha256 TEXT NOT NULL,
            request_json TEXT NOT NULL, PRIMARY KEY(tenant, environment, key));
    """)
    legacy = conn.execute(
        'SELECT key, generation, manifest_sha256, request_json FROM receipts').fetchall()
    state.begin(conn)
    for row in legacy:
        conn.execute('INSERT OR REPLACE INTO receipts_v2(tenant,environment,key,generation,'
                     'manifest_sha256,request_json) VALUES(?,?,?,?,?,?)',
                     (LEGACY_TENANT, LEGACY_ENVIRONMENT, row['key'], row['generation'],
                      row['manifest_sha256'], row['request_json']))
    conn.execute('DROP TABLE receipts')
    conn.execute('ALTER TABLE receipts_v2 RENAME TO receipts')
    if crash_at == 'migration_after_copy':
        conn.execute('COMMIT')
        _crash('migration_after_copy')
    conn.execute('PRAGMA user_version=2')
    conn.execute('COMMIT')
    state.ensure_schema(conn)
    return {'from': 1, 'to': 2, 'migrated': len(legacy)}


# --------------------------------------------------------------------------- #
# publish (loopback receiver protocol)
# --------------------------------------------------------------------------- #
class _NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError('publication redirect refused')


def _publication_base(url):
    from urllib.parse import urlsplit, urlunsplit
    if not isinstance(url, str):
        raise ValueError('publication URL must be a string')
    parsed = urlsplit(url)
    if parsed.scheme != 'http' or parsed.hostname not in ('127.0.0.1', 'localhost', '::1'):
        raise ValueError('refusing non-loopback HTTP publication target')
    if parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError('publication URL may not contain credentials, query, or fragment')
    return urlunsplit((parsed.scheme, parsed.netloc, parsed.path.rstrip('/') + '/', '', ''))


def _receiver_request(url, body=None):
    # No ambient proxy and no redirect may move this loopback-only effect.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), _NoRedirect())
    request = urllib.request.Request(url, data=body,
                                    headers={'Content-Type': 'application/json'},
                                    method='POST' if body is not None else 'GET')
    with opener.open(request, timeout=10) as response:
        if response.status != 200:
            raise ValueError('unexpected receiver status %s' % response.status)
        payload = response.read(65537)
        if len(payload) > 65536:
            raise ValueError('receiver acknowledgement exceeds limit')
        return json.loads(payload)


def _validate_ack(ack, body):
    if not pointer_valid(ack) or canonical_bytes(ack) != body:
        raise ValueError('receiver acknowledgement does not match original publication')
    return ack


def _query_publication(base, tenant, environment, key, body):
    from urllib.parse import urlencode
    query = urlencode({'tenant': tenant, 'environment': environment, 'key': key})
    try:
        return _validate_ack(_receiver_request(base + 'receipt?' + query), body)
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return None
        raise ValueError('receiver query failed with HTTP %s' % exc.code) from exc
    except (urllib.error.URLError, OSError) as exc:
        raise ValueError('publication outcome remains unknown') from exc


def publish(repo, tenant, environment, key, url):
    """An outbox identity binds immutable body AND receiver before any network IO.

    All pending attempts query first, including the first invocation. Holding the
    SQLite writer lock during bounded requests prevents concurrent publishers
    from both observing absent and POSTing. A killed process releases that lock;
    the durable pending row requires the next process to query the original ID.
    """
    conn = repo.conn
    base = _publication_base(url)
    state.begin(conn)
    try:
        row = state.read_receipt(conn, tenant, environment, key)
        if row is None:
            raise ValueError('no committed receipt for publication')
        receipt = receipt_obj(tenant, environment, key, row['generation'], row['manifest_sha256'])
        body = canonical_bytes(receipt)
        existing = state.read_outbox(conn, tenant, environment, key)
        if existing is None:
            state.put_outbox(conn, tenant, environment, key, body.decode('utf-8'), 'pending', url=base)
        elif existing['body'].encode('utf-8') != body or existing['url'] != base:
            raise ValueError('publication identity bound to different body or URL')
        state.commit(conn)  # pending intent survives death at ANY network boundary
    except BaseException:
        state.rollback(conn)
        raise

    state.begin(conn)
    try:
        row = state.read_outbox(conn, tenant, environment, key)
        if row['status'] == 'acknowledged':
            ack = _validate_ack(json.loads(row['ack']), body)
            state.commit(conn)
            return {'published': True, 'acknowledgement': ack, 'receipt': receipt, 'cached': True}
        if row['status'] == 'conflict':
            raise ValueError('publication identity has a durable conflict')
        ack = _query_publication(base, tenant, environment, key, body)
        queried = ack is not None
        if ack is None:  # the ONLY path to POST is an actual HTTP 404
            try:
                ack = _validate_ack(_receiver_request(base + 'publish', body), body)
            except urllib.error.HTTPError as exc:
                if exc.code == 409:
                    state.put_outbox(conn, tenant, environment, key, body.decode('utf-8'), 'conflict')
                    state.commit(conn)
                    raise ValueError('publication conflict (409)') from exc
                # No inline blind retry. The next invocation queries this ID.
                raise ValueError('publication POST outcome requires reconciliation') from exc
            except (urllib.error.URLError, OSError) as exc:
                # Try one bounded query after a lost response. If definitely
                # absent, leave pending; a caller can retry via GET then POST.
                ack = _query_publication(base, tenant, environment, key, body)
                if ack is None:
                    raise ValueError('publication absent after failed POST; retry may reconcile') from exc
                queried = True
        state.put_outbox(conn, tenant, environment, key, body.decode('utf-8'), 'acknowledged',
                         canonical_bytes(ack).decode('utf-8'))
        state.commit(conn)
        return {'published': True, 'acknowledgement': ack, 'receipt': receipt, 'reconciled': queried}
    except BaseException:
        state.rollback(conn)
        raise
