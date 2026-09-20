"""Independent disk/SQLite fixtures for the separately ASSISTED auditor.

PACKAGE_AUDITOR_WORKSPACE may point at the frozen model workspace for red
calibration. Tests invoke the CLI; no candidate helpers define the oracle.
"""
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import unittest


WORK = Path(os.environ.get('PACKAGE_AUDITOR_WORKSPACE',
            str(Path(__file__).resolve().parents[1] / 'auditor_assisted'))).resolve()


def raw(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'),
                      ensure_ascii=False, allow_nan=False).encode()


def sha(value):
    return hashlib.sha256(value).hexdigest()


class AssistedAuditor(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='package-auditor-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'root'
        for directory in ('objects', 'manifests', 'deployments'):
            (self.root / directory).mkdir(parents=True)
        with closing(sqlite3.connect(self.root / 'repo.sqlite')) as db:
            db.executescript('''
                CREATE TABLE receipts(tenant,environment,key,generation,manifest_sha256,request_json);
                CREATE TABLE current(tenant,environment,receipt_json);
            ''')

    def add(self, key='first', generation=1, manifest_scope='tenant-a'):
        content = ('payload-%s' % key).encode()
        digest = sha(content)
        (self.root / 'objects' / digest).write_bytes(content)
        manifest = dict(tenant=manifest_scope, environment='prod',
                        packages=[dict(name='pkg', version=generation, sha256=digest)])
        data = raw(manifest)
        md = sha(data)
        (self.root / 'manifests' / (md + '.json')).write_bytes(data)
        receipt = dict(tenant='tenant-a', environment='prod', key=key,
                       generation=generation, manifest_sha256=md)
        with closing(sqlite3.connect(self.root / 'repo.sqlite')) as db:
            db.execute('INSERT INTO receipts VALUES(?,?,?,?,?,?)',
                       ('tenant-a', 'prod', key, generation, md, '{}'))
            db.execute('DELETE FROM current')
            db.execute('INSERT INTO current VALUES(?,?,?)', ('tenant-a', 'prod', raw(receipt).decode()))
            db.commit()
        pointer = self.root / 'deployments/tenant-a/prod/current.json'
        pointer.parent.mkdir(parents=True, exist_ok=True)
        pointer.write_bytes(raw(receipt))
        return receipt, digest

    def snapshot(self):
        # Include the parent: a malformed SQLite URI can create a new database
        # OUTSIDE the audited root (e.g. truncating a path at a '#' character).
        base = Path(self.temp.name)
        return {str(path.relative_to(base)): sha(path.read_bytes())
                for path in base.rglob('*') if path.is_file()}

    def audit(self, expected):
        before = self.snapshot()
        result = subprocess.run([sys.executable, '-B', '-m', 'app.audit', '--root', str(self.root)],
                                cwd=WORK, capture_output=True, text=True, encoding='utf-8', timeout=15)
        self.assertEqual(self.snapshot(), before, 'audit mutated snapshot/parent bytes or file membership')
        self.assertEqual(result.stderr, '', result.stderr)
        value = json.loads(result.stdout)
        self.assertIs(type(value['ok']), bool)
        self.assertEqual(value['ok'], expected, value)
        self.assertEqual(result.returncode, 0 if expected else 1, result.stdout)
        self.assertEqual(bool(value['errors']), not expected, value)
        return value

    def test_valid_historical_receipts_count_two(self):
        self.add()
        self.add('second', 2)
        self.assertEqual(self.audit(True)['receipts'], 2)

    def test_manifest_scope_is_bound_to_receipt(self):
        self.add(manifest_scope='tenant-b')
        self.audit(False)

    def test_manifest_scope_shape_is_validated(self):
        self.add(manifest_scope=123)
        self.audit(False)

    def test_old_durable_pointer_cannot_override_sqlite_current(self):
        first, _ = self.add()
        self.add('second', 2)
        (self.root / 'deployments/tenant-a/prod/current.json').write_bytes(raw(first))
        self.audit(False)

    def test_invalid_directory_cannot_hide_orphan_pointer(self):
        first, _ = self.add()
        hidden = self.root / 'deployments/tenant_bad/prod/current.json'
        hidden.parent.mkdir(parents=True)
        hidden.write_bytes(raw(dict(first, key='ghost')))
        self.audit(False)

    def test_object_directory_link_cannot_escape_snapshot(self):
        _, digest = self.add()
        outside = Path(self.temp.name) / 'outside'
        outside.mkdir()
        # The generated objects directory contains exactly this known file.
        (self.root / 'objects' / digest).replace(outside / digest)
        (self.root / 'objects').rmdir()
        if os.name == 'nt':
            quote = lambda value: "'" + str(value).replace("'", "''") + "'"
            command = 'New-Item -ItemType Junction -Path %s -Target %s | Out-Null' % (
                quote(self.root / 'objects'), quote(outside))
            result = subprocess.run(['powershell', '-NoProfile', '-Command', command],
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
        else:
            (self.root / 'objects').symlink_to(outside, target_is_directory=True)
        before_external = (outside / digest).read_bytes()
        self.audit(False)
        self.assertEqual((outside / digest).read_bytes(), before_external)

    def test_historical_corruption_with_actual_durable_history_is_detected(self):
        first, _ = self.add()
        self.add('second', 2)
        (self.root / 'manifests' / (first['manifest_sha256'] + '.json')).write_bytes(b'corrupt')
        self.audit(False)

    def test_missing_current_authority_is_not_a_pass(self):
        self.add()
        with closing(sqlite3.connect(self.root / 'repo.sqlite')) as db:
            db.execute('DROP TABLE current')
            db.commit()
        self.audit(False)

    def test_missing_current_pointer_is_not_a_pass(self):
        self.add()
        (self.root / 'deployments/tenant-a/prod/current.json').unlink()
        self.audit(False)

    def test_wal_sidecar_is_refused_without_modification(self):
        self.add()
        (self.root / 'repo.sqlite-wal').write_bytes(b'possible committed WAL')
        self.audit(False)

    def test_invalid_json_number_produces_single_error_document(self):
        first, _ = self.add()
        bad = b'{"environment":"prod","packages":[],"tenant":NaN}'
        digest = sha(bad)
        (self.root / 'manifests' / (digest + '.json')).write_bytes(bad)
        with closing(sqlite3.connect(self.root / 'repo.sqlite')) as db:
            db.execute('UPDATE receipts SET manifest_sha256=?', (digest,))
            db.execute('UPDATE current SET receipt_json=?', (raw(dict(first, manifest_sha256=digest)).decode(),))
            db.commit()
        (self.root / 'deployments/tenant-a/prod/current.json').write_bytes(raw(dict(first, manifest_sha256=digest)))
        self.audit(False)

    def test_noncanonical_pointer_is_rejected(self):
        first, _ = self.add()
        (self.root / 'deployments/tenant-a/prod/current.json').write_text(json.dumps(first, indent=2))
        self.audit(False)

    def test_uri_special_character_in_root_remains_valid(self):
        self.add()
        destination = self.root.parent / 'snapshot#one'
        self.root.rename(destination)
        self.root = destination
        self.audit(True)


if __name__ == '__main__':
    unittest.main()
