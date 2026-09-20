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
    if isinstance(pointer['generation'], bool) or not isinstance(pointer['generation'], int):
        return False
    digest = pointer['manifest_sha256']
    if not isinstance(digest, str) or len(digest) != 64:
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
    conn = repo.conn
    root = repo.root
    solution = repo._resolve(requirements)
    packages = prepare(root, repo.catalog_path, repo.catalog, solution)

    state.begin(conn)
    try:
        existing = state.read_receipt(conn, tenant, environment, key)
        if existing is not None:
            if existing['request_json'] != _request_json(requirements, expected_generation):
                raise ValueError('identity %s/%s/%s bound to different content'
                                 % (tenant, environment, key))
            receipt = receipt_obj(tenant, environment, key, existing['generation'],
                                  existing['manifest_sha256'])
            state.commit(conn)
            _heal_pointer(root, tenant, environment, receipt)
            return receipt

        current = state.read_current(conn, tenant, environment)
        current_gen = json.loads(current['receipt_json'])['generation'] if current else 0
        if expected_generation is not None and expected_generation != current_gen:
            raise ValueError('expected_generation %r != current %r'
                             % (expected_generation, current_gen))

        manifest_digest = write_manifest(root, tenant, environment, packages)
        generation = current_gen + 1
        receipt = receipt_obj(tenant, environment, key, generation, manifest_digest)
        record = {'tenant': tenant, 'environment': environment, 'key': key, 'status': 'pending',
                  'generation': generation, 'manifest_sha256': manifest_digest,
                  'request_json': _request_json(requirements, expected_generation)}
        journal_write(root, tenant, environment, key, record)
        state.journal_put(conn, tenant, environment, key, {
            'tenant': tenant, 'environment': environment, 'key': key, 'status': 'pending',
            'old_generation': current_gen if current else None,
            'old_manifest': (json.loads(current['receipt_json'])['manifest_sha256'] if current else None),
            'old_receipt': (current['receipt_json'] if current else None),
            'new_generation': generation, 'new_manifest': manifest_digest,
            'new_receipt': canonical_bytes(receipt).decode('utf-8')})
        state.commit(conn)  # durable before pointer

        if crash_at == 'before_pointer':
            _crash('before_pointer')
        atomic_write_json(pointer_path(root, tenant, environment), receipt)
        if crash_at == 'after_pointer':
            _crash('after_pointer')

        state.begin(conn)
        state.put_current(conn, tenant, environment, receipt)
        state.insert_receipt(conn, tenant, environment, key, generation, manifest_digest,
                             _request_json(requirements, expected_generation))
        state.journal_delete(conn, tenant, environment, key)
        state.commit(conn)
        journal_clear(root, tenant, environment, key)
        return receipt
    except BaseException:
        state.rollback(conn)
        raise


def _heal_pointer(root, tenant, environment, receipt):
    if read_pointer(root, tenant, environment) != receipt:
        atomic_write_json(pointer_path(root, tenant, environment), receipt)


# --------------------------------------------------------------------------- #
# recovery
# --------------------------------------------------------------------------- #
def reconcile(repo):
    """Reconcile journal entries (DB and disk) before another mutation."""
    conn = repo.conn
    healed = 0
    db_records = {}
    for row in state.journals(conn):
        try:
            db_records[(row['tenant'], row['environment'], row['key'])] = \
                json.loads(row['new_receipt'])
        except ValueError:
            db_records[(row['tenant'], row['environment'], row['key'])] = None
    disk_records = {}
    for rec in _disk_journal_records(repo.root):
        if isinstance(rec, dict) and 'tenant' in rec:
            disk_records[(rec['tenant'], rec['environment'], rec['key'])] = rec

    for ident in set(db_records) | set(disk_records):
        tenant, environment, key = ident
        record = disk_records.get(ident) or db_records.get(ident)
        pointer = read_pointer(repo.root, tenant, environment)
        digest = record.get('manifest_sha256') if isinstance(record, dict) else None
        receipt = receipt_obj(tenant, environment, key, record['generation'], digest) \
            if isinstance(record, dict) and 'generation' in record else None
        if pointer is None or not pointer_valid(pointer) or pointer != receipt \
                or not _manifest_intact(repo.root, digest):
            _discard(repo, tenant, environment, key)
            continue
        state.begin(conn)
        state.put_current(conn, tenant, environment, pointer)
        if state.read_receipt(conn, tenant, environment, key) is None:
            state.insert_receipt(conn, tenant, environment, key, pointer['generation'], digest,
                                 record.get('request_json', '{}'))
        state.journal_delete(conn, tenant, environment, key)
        state.commit(conn)
        journal_clear(repo.root, tenant, environment, key)
        healed += 1
    return healed


def _manifest_intact(root, digest):
    return isinstance(digest, str) and len(digest) == 64 \
        and read_verified(manifest_path(root, digest), digest) is not None


def _discard(repo, tenant, environment, key):
    state.begin(repo.conn)
    state.journal_delete(repo.conn, tenant, environment, key)
    state.commit(repo.conn)
    journal_clear(repo.root, tenant, environment, key)


# --------------------------------------------------------------------------- #
# GC
# --------------------------------------------------------------------------- #
def gc(repo):
    root = repo.root
    conn = repo.conn
    try:
        rows = state.all_receipts(conn)
        journals = state.journals(conn)
    except Exception:
        return {'deleted_objects': 0, 'deleted_manifests': 0, 'deferred': True}
    referenced = {row['manifest_sha256'] for row in rows}
    for row in journals:
        try:
            rec = json.loads(row['new_receipt'])
        except ValueError:
            continue
        if isinstance(rec, dict) and rec.get('manifest_sha256'):
            referenced.add(rec['manifest_sha256'])
    live_blobs = set()
    for digest in referenced:
        data = read_verified(manifest_path(root, digest), digest)
        if data is None:
            continue
        for pkg in json.loads(data.decode('utf-8'))['packages']:
            live_blobs.add(pkg['sha256'])
    deleted_objects = deleted_manifests = 0
    obj_dir = Path(root) / 'objects'
    if obj_dir.is_dir():
        for entry in obj_dir.iterdir():
            if entry.name in live_blobs:
                continue
            try:
                os.unlink(entry)
                deleted_objects += 1
            except OSError:
                pass
    man_dir = Path(root) / 'manifests'
    if man_dir.is_dir():
        for entry in man_dir.iterdir():
            digest = entry.name[:-5] if entry.name.endswith('.json') else entry.name
            if digest in referenced:
                continue
            try:
                os.unlink(entry)
                deleted_manifests += 1
            except OSError:
                pass
    return {'deleted_objects': deleted_objects, 'deleted_manifests': deleted_manifests,
            'deferred': False}


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
def _loopback(url):
    from urllib.parse import urlparse
    return urlparse(url).hostname in ('127.0.0.1', 'localhost', '::1')


def publish(repo, tenant, environment, key, url):
    conn = repo.conn
    row = state.read_receipt(conn, tenant, environment, key)
    if row is None:
        raise ValueError('no committed receipt for %s/%s/%s' % (tenant, environment, key))
    receipt = receipt_obj(tenant, environment, key, row['generation'], row['manifest_sha256'])
    body = canonical_bytes(receipt)
    state.put_outbox(conn, tenant, environment, key, body.decode('utf-8'), 'pending')
    if not _loopback(url):
        raise ValueError('refusing non-loopback publication target %r' % (url,))
    base = url if url.endswith('/') else url + '/'
    try:
        req = urllib.request.Request(base + 'publish', data=body,
                                     headers={'Content-Type': 'application/json'}, method='POST')
        with urllib.request.urlopen(req, timeout=10) as resp:
            ack = json.loads(resp.read().decode('utf-8'))
    except urllib.error.HTTPError as exc:
        if exc.code == 409:
            state.put_outbox(conn, tenant, environment, key, body.decode('utf-8'), 'conflict')
            raise ValueError('publication conflict (409) for %s/%s/%s' % (tenant, environment, key))
        return _reconcile_publish(repo, tenant, environment, key, base, body)
    except (urllib.error.URLError, OSError):
        return _reconcile_publish(repo, tenant, environment, key, base, body)
    state.put_outbox(conn, tenant, environment, key, body.decode('utf-8'), 'acknowledged',
                     json.dumps(ack, sort_keys=True, separators=(',', ':')))
    return {'published': True, 'acknowledgement': ack, 'receipt': receipt}


def _reconcile_publish(repo, tenant, environment, key, base, body):
    from urllib.parse import quote
    url = base + 'receipt?tenant=%s&environment=%s&key=%s' % (quote(tenant), quote(environment),
                                                              quote(key))
    try:
        with urllib.request.urlopen(url, timeout=10) as resp:
            existing = json.loads(resp.read().decode('utf-8'))
    except urllib.error.HTTPError as exc:
        if exc.code == 404:
            return _retry_publish(repo, tenant, environment, key, base, body)
        raise ValueError('receiver error %r during reconciliation' % (exc.code,))
    except (urllib.error.URLError, OSError):
        raise ValueError('cannot determine publication outcome for %s/%s/%s'
                         % (tenant, environment, key))
    state.put_outbox(repo.conn, tenant, environment, key, body.decode('utf-8'), 'acknowledged',
                     json.dumps(existing, sort_keys=True, separators=(',', ':')))
    return {'published': True, 'acknowledgement': existing, 'reconciled': True}


def _retry_publish(repo, tenant, environment, key, base, body):
    req = urllib.request.Request(base + 'publish', data=body,
                                 headers={'Content-Type': 'application/json'}, method='POST')
    with urllib.request.urlopen(req, timeout=10) as resp:
        ack = json.loads(resp.read().decode('utf-8'))
    state.put_outbox(repo.conn, tenant, environment, key, body.decode('utf-8'), 'acknowledged',
                     json.dumps(ack, sort_keys=True, separators=(',', ':')))
    return {'published': True, 'acknowledgement': ack, 'retried': True}
