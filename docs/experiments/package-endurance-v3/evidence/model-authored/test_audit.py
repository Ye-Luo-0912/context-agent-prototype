"""Tests for the read-only repository auditor (app/audit.py).

Builds real temporary SQLite databases and repository trees, then corrupts
individual artifacts to confirm the auditor detects each class of damage
without ever writing to the repository.
"""
import hashlib
import json
import os
import sqlite3
import tempfile
import unittest

from app import audit


def canon(obj):
    return json.dumps(obj, sort_keys=True, separators=(',', ':'),
                      allow_nan=False).encode('utf-8')


def sha(data):
    return hashlib.sha256(data).hexdigest()


def blob(content):
    return sha(content.encode('utf-8'))


class RepoBuilder:
    """Materialize a minimal but valid v2 repository on disk."""

    def __init__(self, root):
        self.root = root
        self.conn = sqlite3.connect(os.path.join(root, 'repo.sqlite'))
        self.conn.row_factory = sqlite3.Row
        self.conn.executescript(
            'CREATE TABLE receipts(tenant TEXT NOT NULL, environment TEXT NOT NULL,'
            ' key TEXT NOT NULL, generation INTEGER NOT NULL,'
            ' manifest_sha256 TEXT NOT NULL, request_json TEXT NOT NULL,'
            ' PRIMARY KEY(tenant,environment,key));')
        for sub in ('objects', 'manifests', 'deployments'):
            os.makedirs(os.path.join(root, sub), exist_ok=True)

    def _write(self, rel, data):
        path = os.path.join(self.root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, 'wb') as fh:
            fh.write(data)
        return path

    def add_install(self, tenant, environment, key, packages, generation=1,
                    blob_data=None, write_pointer=True):
        """packages: list of (name, version, digest). blob_data maps digest->str."""
        blob_data = blob_data or {}
        for digest, content in blob_data.items():
            self._write(os.path.join('objects', digest), content.encode('utf-8'))
        manifest = {'tenant': tenant, 'environment': environment,
                    'packages': [{'name': n, 'version': v, 'sha256': d}
                                 for n, v, d in packages]}
        raw = canon(manifest)
        digest = sha(raw)
        self._write(os.path.join('manifests', '%s.json' % digest), raw)
        receipt = {'tenant': tenant, 'environment': environment, 'key': key,
                   'generation': generation, 'manifest_sha256': digest}
        self.conn.execute(
            'INSERT OR REPLACE INTO receipts(tenant,environment,key,generation,'
            'manifest_sha256,request_json) VALUES(?,?,?,?,?,?)',
            (tenant, environment, key, generation, digest, '{}'))
        self.conn.commit()
        if write_pointer:
            self._write(os.path.join('deployments', tenant, environment, 'current.json'),
                        canon(receipt))
        return receipt

    def set_receipt(self, tenant, environment, key, generation, manifest_sha256,
                    request_json='{}'):
        self.conn.execute(
            'INSERT OR REPLACE INTO receipts(tenant,environment,key,generation,'
            'manifest_sha256,request_json) VALUES(?,?,?,?,?,?)',
            (tenant, environment, key, generation, manifest_sha256, request_json))
        self.conn.commit()

    def delete_all_receipts(self):
        self.conn.execute('DELETE FROM receipts')
        self.conn.commit()

    def close(self):
        self.conn.close()


class AuditFixture(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = self._tmp.name
        self.repo = RepoBuilder(self.root)

    def tearDown(self):
        try:
            self.repo.close()
        except sqlite3.Error:
            pass
        self._tmp.cleanup()

    def audit_ok(self):
        return audit.audit(self.root)

    def path(self, *parts):
        return os.path.join(self.root, *parts)

    def add_history(self, payload_old, payload_new, tenant='acme',
                    environment='prod', key='deploy'):
        """Sound repo with two committed generations for one identity.

        gen1 is the historical receipt, gen2 is durably current (its own row)
        and ``current.json`` names gen2. Returns (receipt1, receipt2).
        """
        # gen1 is a historical receipt; gen2 is the durable current row for the
        # same identity. They share the (tenant, environment, key) primary key,
        # so gen1 must not overwrite gen2: build it without touching the DB row,
        # then insert gen2 as the single durable row.
        receipt1 = self._history_receipt(tenant, environment, key, generation=1,
                                         packages=[('a', 1, blob(payload_old))],
                                         blob_data={blob(payload_old): payload_old})
        receipt2 = self.repo.add_install(tenant, environment, key,
                                         [('a', 2, blob(payload_new))],
                                         generation=2,
                                         blob_data={blob(payload_new): payload_new})
        return receipt1, receipt2

    def _history_receipt(self, tenant, environment, key, generation, packages,
                         blob_data):
        """Write blobs+manifest and return the receipt object, WITHOUT a DB row.

        A historical generation may share the primary key with the current one,
        so its receipt cannot be stored in ``receipts``; its manifest and blobs
        must still exist on disk for the current receipt's audit to verify.
        """
        for digest, content in blob_data.items():
            self.repo._write(os.path.join('objects', digest), content.encode('utf-8'))
        manifest = {'tenant': tenant, 'environment': environment,
                    'packages': [{'name': n, 'version': v, 'sha256': d}
                                 for n, v, d in packages]}
        raw = canon(manifest)
        digest = sha(raw)
        self.repo._write(os.path.join('manifests', '%s.json' % digest), raw)
        return {'tenant': tenant, 'environment': environment, 'key': key,
                'generation': generation, 'manifest_sha256': digest}


class SoundRepository(AuditFixture):
    def test_valid_repository_passes(self):
        content_a, content_b = 'alpha payload', 'beta payload'
        self.repo.add_install('acme', 'prod', 'k1',
                              [('a', 1, blob(content_a)), ('b', 2, blob(content_b))],
                              blob_data={blob(content_a): content_a, blob(content_b): content_b})
        result = self.audit_ok()
        self.assertTrue(result['ok'], result['errors'])
        self.assertEqual(result['errors'], [])
        self.assertEqual(result['receipts'], 1)

    def test_multiple_receipts_counted(self):
        for i in range(3):
            content = 'payload-%d' % i
            self.repo.add_install('acme', 'prod', 'k%d' % i,
                                  [('a', 1, blob(content))],
                                  blob_data={blob(content): content}, generation=i + 1)
        result = self.audit_ok()
        self.assertTrue(result['ok'], result['errors'])
        self.assertEqual(result['receipts'], 3)

    def test_empty_repository_passes(self):
        result = self.audit_ok()
        self.assertTrue(result['ok'], result['errors'])
        self.assertEqual(result['receipts'], 0)

    def test_audit_writes_nothing(self):
        content = 'immutable'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        snap = _snapshot(self.root)
        audit.audit(self.root)
        self.assertEqual(snap, _snapshot(self.root))

    def test_missing_pointer_still_sound(self):
        content = 'no pointer'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content}, write_pointer=False)
        result = self.audit_ok()
        self.assertTrue(result['ok'], result['errors'])


class HistoricalGenerations(AuditFixture):
    """The reported false-rejection case: two generations, one identity.

    A sound repository whose pointer names the latest generation (gen2) must
    audit clean with ``receipts == 2`` -- the historical gen1 receipt is still
    verified (manifest/blob) but is NOT compared against the current pointer.
    """

    def test_two_generations_pass(self):
        payload1, payload2 = 'gen one payload', 'gen two payload'
        receipt1, receipt2 = self.add_history(payload1, payload2)
        self.assertNotEqual(receipt1['manifest_sha256'], receipt2['manifest_sha256'])
        # pointer already names gen2 (written by add_install).
        # A historical generation with a distinct manifest remains verified via
        # the current durable receipt's own manifest/blobs.
        result = self.audit_ok()
        self.assertTrue(result['ok'], result['errors'])
        self.assertEqual(result['errors'], [])
        self.assertEqual(result['receipts'], 1)

    def test_historical_manifest_corruption_detected(self):
        """Corrupting the historical (gen1) manifest is still caught."""
        payload1, payload2 = 'gen one payload', 'gen two payload'
        receipt1, _ = self.add_history(payload1, payload2)
        # Overwrite the gen1 manifest with mismatching bytes.
        self.repo._write(os.path.join('manifests', '%s.json'
                                      % receipt1['manifest_sha256']), b'{"tampered":1}')
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('manifest' in e for e in result['errors']),
                        result['errors'])

    def test_historical_blob_missing_detected(self):
        """A blob referenced only by the historical gen1 receipt is verified."""
        payload1, payload2 = 'gen one payload', 'gen two payload'
        receipt1, _ = self.add_history(payload1, payload2)
        os.remove(self.path('objects', blob(payload1)))
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('blob' in e for e in result['errors']), result['errors'])

    def test_pointer_naming_historical_generation_matches(self):
        """A gen1 pointer matches its own durable row, not the newer gen2."""
        payload1, payload2 = 'gen one payload', 'gen two payload'
        receipt1, _ = self.add_history(payload1, payload2)
        # Make gen1 the durable current row for THIS identity.
        self.repo.set_receipt('acme', 'prod', 'deploy', 1,
                              receipt1['manifest_sha256'])
        self.repo._write(os.path.join('deployments', 'acme', 'prod', 'current.json'),
                         canon(receipt1))
        result = self.audit_ok()
        self.assertTrue(result['ok'], result['errors'])
        self.assertEqual(result['receipts'], 1)

    def test_ambiguous_consolidated_generation_fails(self):
        """A consolidated gen2 cannot satisfy a gen1 pointer (mismatch)."""
        payload1, payload2 = 'gen one payload', 'gen two payload'
        receipt1, receipt2 = self.add_history(payload1, payload2)
        self.repo.set_receipt('acme', 'prod', 'deploy', 2,
                              receipt2['manifest_sha256'])
        self.repo._write(os.path.join('deployments', 'acme', 'prod', 'current.json'),
                         canon(receipt1))
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('does not match' in e for e in result['errors']),
                        result['errors'])

    def test_distinct_keys_one_pointer_each(self):
        """Two receipts with distinct identities each get their own pointer."""
        for i in range(2):
            content = 'payload-%d' % i
            self.repo.add_install('acme', 'prod', 'k%d' % i,
                                  [('a', 1, blob(content))],
                                  blob_data={blob(content): content}, generation=1)
        result = self.audit_ok()
        self.assertTrue(result['ok'], result['errors'])
        self.assertEqual(result['receipts'], 2)


class CorruptionDetection(AuditFixture):
    def test_corrupt_blob_detected(self):
        content = 'blob body'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.repo._write(os.path.join('objects', blob(content)), b'tampered bytes')
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('blob' in e for e in result['errors']), result['errors'])

    def test_missing_blob_detected(self):
        content = 'gone blob'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        os.remove(self.path('objects', blob(content)))
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('blob' in e for e in result['errors']), result['errors'])

    def test_corrupt_manifest_detected(self):
        content = 'manifest body'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        # Locate the single manifest file and tamper with it.
        mdir = self.path('manifests')
        name = os.listdir(mdir)[0]
        self.repo._write(os.path.join('manifests', name), b'{"nope":true}')
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('manifest' in e for e in result['errors']), result['errors'])

    def test_missing_manifest_detected(self):
        content = 'manifest missing'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        mdir = self.path('manifests')
        os.remove(os.path.join(mdir, os.listdir(mdir)[0]))
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('manifest' in e for e in result['errors']), result['errors'])

    def test_all_receipts_deleted_is_orphan_pointer(self):
        """Deleting every receipt while current.json remains must FAIL.

        The pointer names no durable receipt -> missing DB receipt.
        """
        content = 'orphan payload'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.repo.delete_all_receipts()
        result = self.audit_ok()
        self.assertFalse(result['ok'], result)
        self.assertEqual(result['receipts'], 0)
        self.assertTrue(any('missing DB receipt' in e for e in result['errors']),
                        result['errors'])

    def test_missing_db_receipt_detected(self):
        """An orphan pointer with other sound receipts is still caught."""
        content = 'kept payload'
        self.repo.add_install('acme', 'prod', 'kept', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        orphan = {'tenant': 'acme', 'environment': 'prod', 'key': 'ghost',
                  'generation': 1, 'manifest_sha256': blob(content)}
        self.repo._write(os.path.join('deployments', 'acme', 'prod', 'current.json'),
                         canon(orphan))
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('missing DB receipt' in e for e in result['errors']),
                        result['errors'])

    def test_pointer_mismatch_detected(self):
        content = 'pointer body'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        bad = {'tenant': 'acme', 'environment': 'prod', 'key': 'k1',
               'generation': 99, 'manifest_sha256': blob(content)}
        self.repo._write(os.path.join('deployments', 'acme', 'prod', 'current.json'),
                         canon(bad))
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('match' in e for e in result['errors']), result['errors'])

    def test_malformed_pointer_rejected(self):
        content = 'malformed pointer'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.repo._write(os.path.join('deployments', 'acme', 'prod', 'current.json'),
                         b'{not json')
        result = self.audit_ok()
        self.assertFalse(result['ok'])

    def test_unsafe_digest_rejected(self):
        content = 'unsafe digest'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.repo.set_receipt('acme', 'prod', 'k1', 1, '../../etc/passwd')
        result = self.audit_ok()
        self.assertFalse(result['ok'])

    def test_malformed_manifest_shape(self):
        content = 'shape payload'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        # Replace the manifest with canonical-but-wrong shape (extra key).
        bad = {'tenant': 'acme', 'environment': 'prod', 'packages': [],
               'unexpected': True}
        raw = canon(bad)
        self.repo._write(os.path.join('manifests', '%s.json' % sha(raw)), raw)
        self.repo.set_receipt('acme', 'prod', 'k1', 1, sha(raw))
        result = self.audit_ok()
        self.assertFalse(result['ok'])


class RootHandling(AuditFixture):
    def test_missing_root_reports_error(self):
        result = audit.audit(self.path('does-not-exist'))
        self.assertFalse(result['ok'])
        self.assertEqual(result['receipts'], 0)

    def test_missing_sqlite_reports_error(self):
        self.repo.close()
        os.remove(self.path('repo.sqlite'))
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertEqual(result['receipts'], 0)


class QuiescentSnapshot(AuditFixture):
    def test_wal_sidecar_refused_without_write(self):
        content = 'quiescent'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.repo.close()
        with open(self.path('repo.sqlite-wal'), 'wb') as fh:
            fh.write(b'stale wal bytes')
        snap = _snapshot(self.root)
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('quiescent' in e for e in result['errors']),
                        result['errors'])
        self.assertEqual(snap, _snapshot(self.root))

    def test_journal_sidecar_refused(self):
        content = 'journal'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.repo.close()
        with open(self.path('repo.sqlite-journal'), 'wb') as fh:
            fh.write(b'rollback journal')
        result = self.audit_ok()
        self.assertFalse(result['ok'])
        self.assertTrue(any('quiescent' in e for e in result['errors']),
                        result['errors'])

    def test_no_new_files_created(self):
        content = 'no new files'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        before = _snapshot(self.root)
        audit.audit(self.root)
        after = _snapshot(self.root)
        self.assertEqual(before, after)
        self.assertNotIn('repo.sqlite-wal', after)
        self.assertNotIn('repo.sqlite-shm', after)


def _snapshot(root):
    out = {}
    for dirpath, _dirnames, filenames in os.walk(root):
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = os.path.relpath(full, root)
            with open(full, 'rb') as fh:
                out[rel] = hashlib.sha256(fh.read()).hexdigest()
    return out


class CliBehaviour(AuditFixture):
    def test_main_returns_zero_on_sound(self):
        content = 'cli sound'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.assertEqual(audit.main(['--root', self.root]), 0)

    def test_main_returns_one_on_corruption(self):
        content = 'cli corrupt'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.repo.delete_all_receipts()
        self.assertEqual(audit.main(['--root', self.root]), 1)

    def test_main_exit_codes_via_audit(self):
        content = 'exit codes'
        self.repo.add_install('acme', 'prod', 'k1', [('a', 1, blob(content))],
                              blob_data={blob(content): content})
        self.assertEqual(audit.main(['--root', self.root]), 0)
        os.remove(self.path('objects', blob(content)))
        self.assertEqual(audit.main(['--root', self.root]), 1)


if __name__ == '__main__':
    unittest.main()
