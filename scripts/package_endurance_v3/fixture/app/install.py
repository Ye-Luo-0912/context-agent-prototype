"""Continue deploy: install/recover/active/receipt (SPEC.md)."""
import hashlib
import json
import os
from pathlib import Path

from app import deploy, state
from app.util import (atomic_write, atomic_write_json, canonical_bytes,
                      check_name, read_json, sha256_file)


def _write_pointer(root, tenant, environment, receipt):
    atomic_write_json(deploy._tenant_dir(root, tenant, environment) / 'current.json', receipt)


def _read_pointer(root, tenant, environment):
    path = deploy._tenant_dir(root, tenant, environment) / 'current.json'
    if not os.path.exists(path):
        return None
    try:
        obj = read_json(path)
    except (OSError, ValueError):
        return None
    if canonical_bytes(obj) != Path(path).read_bytes():
        return None
    return obj


def _count_complete(conn, tenant, environment):
    row = conn.execute(
        'SELECT COUNT(*) AS c FROM receipts WHERE tenant=? AND environment=?',
        (tenant, environment)).fetchone()
    return int(row['c'])


def _prep_objects(root, catalog, catalog_path, solution):
    """Resolve+verify blobs and write canonical manifest; return (digest, packages)."""
    blobs = deploy._resolve_sources(catalog_path, catalog, solution)
    packages = []
    for name in sorted(solution):
        version = solution[name]
        entry = next(e for e in catalog[name] if e['version'] == version)
        digest = entry['sha256']
        deploy._ensure_object(root, digest, blobs[digest])
        packages.append((name, (version, digest)))
    return packages


def _publish_manifest(root, tenant, environment, packages):
    obj = deploy._manifest_obj(tenant, environment, packages)
    data = canonical_bytes(obj)
    digest = hashlib.sha256(data).hexdigest()
    path = deploy._manifest_path(root, digest)
    if os.path.exists(path):
        if Path(path).read_bytes() == data:
            return digest
        raise ValueError('manifest path collision: %s' % digest)
    atomic_write(path, data)
    if Path(path).read_bytes() != data:
        raise ValueError('manifest write corrupted: %s' % digest)
    return digest


class Transaction:
    """One install attempt: begin -> CAS/journal/prep -> commit -> pointer."""

    def __init__(self, repo, tenant, environment, key, requirements, expected_generation):
        self.repo = repo
        self.tenant = tenant
        self.environment = environment
        self.key = key
        self.requirements = requirements
        self.expected_generation = expected_generation
        self.root = repo.root

    def _request_json(self):
        return json.dumps({'requirements': self.requirements,
                           'expected_generation': self.expected_generation},
                          sort_keys=True, separators=(',', ':'), allow_nan=False)

    def install(self, solution, catalog_path, catalog, crash_at):
        root = self.root
        conn = self.repo._conn()
        state.begin(conn)
        try:
            existing = state.read_receipt(conn, self.tenant, self.environment, self.key)
            if existing is not None:
                if existing['request_json'] != self._request_json():
                    raise ValueError('identity %s/%s/%s already bound to different content'
                                     % (self.tenant, self.environment, self.key))
                state.commit(conn)
                self._reconcile_pointer(existing)
                return deploy._receipt_obj(self.tenant, self.environment, self.key,
                                           existing['generation'], existing['manifest_sha256'])

            packages = _prep_objects(root, catalog, catalog_path, solution)
            manifest_digest = _publish_manifest(root, self.tenant, self.environment, packages)

            old = self._current_journal_row(conn)
            if self.expected_generation is not None:
                old_gen = old['generation'] if old else 0
                if old_gen != self.expected_generation:
                    raise ValueError('expected_generation %r does not match current %r'
                                     % (self.expected_generation, old_gen))

            generation = old['generation'] + 1 if old is not None else 1
            receipt = deploy._receipt_obj(self.tenant, self.environment, self.key,
                                          generation, manifest_digest)
            new_json = canonical_bytes(receipt).decode('utf-8')
            journal_row = {
                'tenant': self.tenant, 'environment': self.environment, 'key': self.key,
                'status': 'pending',
                'old_generation': old['generation'] if old else None,
                'old_manifest': old['manifest_sha256'] if old else None,
                'old_receipt': old['receipt_json'] if old else None,
                'new_generation': generation, 'new_manifest': manifest_digest,
                'new_receipt': new_json,
            }
            state.journal_put(conn, journal_row)
            state.commit(conn)  # journal + receipts are durable before the pointer

            if crash_at == 'before_pointer':
                _crash('before_pointer')
            _write_pointer(root, self.tenant, self.environment, receipt)
            if crash_at == 'after_pointer':
                _crash('after_pointer')

            state.begin(conn)
            state.ensure_current_table(conn)
            conn.execute(
                'INSERT OR REPLACE INTO current(tenant,environment,receipt_json) VALUES(?,?,?)',
                (self.tenant, self.environment, new_json))
            conn.execute(
                'INSERT INTO receipts(tenant,environment,key,generation,manifest_sha256,request_json)'
                ' VALUES(?,?,?,?,?,?)',
                (self.tenant, self.environment, self.key, generation, manifest_digest,
                 self._request_json()))
            state.journal_delete(conn, self.tenant, self.environment, self.key)
            state.commit(conn)
            return receipt
        except BaseException:
            state.rollback(conn)
            raise

    def _current_journal_row(self, conn):
        row = conn.execute(
            'SELECT receipt_json, generation, manifest_sha256 FROM current WHERE tenant=? AND environment=?',
            (self.tenant, self.environment)).fetchone()
        if row is not None:
            return {'generation': row['generation'] if 'generation' in row.keys() else None,
                    'manifest_sha256': None, 'receipt_json': row['receipt_json']}
        pointer = _read_pointer(self.root, self.tenant, self.environment)
        if pointer is None:
            return None
        return {'generation': pointer.get('generation'), 'manifest_sha256': pointer.get('manifest_sha256'),
                'receipt_json': json.dumps(pointer, sort_keys=True, separators=(',', ':'))}

    def _reconcile_pointer(self, existing):
        """Ensure pointer exists for a committed receipt (crash healing)."""
        pointer = _read_pointer(self.root, self.tenant, self.environment)
        receipt = deploy._receipt_obj(self.tenant, self.environment, self.key,
                                      existing['generation'], existing['manifest_sha256'])
        if pointer != receipt:
            _write_pointer(self.root, self.tenant, self.environment, receipt)
