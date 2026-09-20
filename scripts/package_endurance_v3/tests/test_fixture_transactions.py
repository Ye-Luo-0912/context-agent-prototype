"""Behavior checks of the ASSISTED fixture, using disk/SQL and real processes.

Set PACKAGE_TEST_APP_SOURCE to an older app directory to calibrate regressions.
All writes go to temporary trees; no experiment evidence is changed.
"""
import hashlib
from contextlib import closing
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest


SOURCE = Path(os.environ.get('PACKAGE_TEST_APP_SOURCE',
              str(Path(__file__).resolve().parents[1] / 'fixture' / 'app')))


def raw(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'),
                      ensure_ascii=False, allow_nan=False).encode('utf-8')


INVOKE = """
import json, sys
from app.repository import Repository
request=json.loads(sys.stdin.read())
try:
    with Repository(sys.argv[1],sys.argv[2]) as repo:
        result=getattr(repo,request['op'])(**request.get('kwargs',{}))
    print(json.dumps({'ok':True,'value':result}))
except Exception as exc:
    print(json.dumps({'ok':False,'error_type':type(exc).__name__,'error':str(exc)}))
"""


class FixtureHarness(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='package-v3-fixture-')
        self.addCleanup(self.temporary.cleanup)
        self.work = Path(self.temporary.name)
        shutil.copytree(SOURCE, self.work / 'app', ignore=shutil.ignore_patterns('__pycache__'))
        self.root = self.work / 'repo'
        fixtures = self.work / 'fixtures'
        (fixtures / 'blobs').mkdir(parents=True)
        self.catalog = {}
        for version in (1, 2, 3):
            body = ('package bytes version %d\n' % version).encode()
            digest = hashlib.sha256(body).hexdigest()
            (fixtures / 'blobs' / digest).write_bytes(body)
            self.catalog.setdefault('pkg', []).append(
                dict(version=version, sha256=digest, deps={}))
        self.catalog_path = fixtures / 'catalog.json'
        self.catalog_path.write_bytes(raw(self.catalog))
        self.children = []
        self.addCleanup(self.reap)

    def reap(self):
        for child in self.children:
            if child.poll() is None:
                child.kill()
            child.communicate(timeout=10)

    def request(self, key='first', version=1, **extra):
        return dict(tenant='tenant-a', environment='prod', key=key,
                    requirements={'pkg': dict(min=version, max=version + 1)}, **extra)

    def spawn(self, operation, **kwargs):
        child = subprocess.Popen([sys.executable, '-c', INVOKE, str(self.root), str(self.catalog_path)],
                                 cwd=self.work, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True, encoding='utf-8')
        child.stdin.write(json.dumps(dict(op=operation, kwargs=kwargs)))
        child.stdin.close()
        child.stdin = None
        self.children.append(child)
        return child

    def finish(self, child):
        stdout, stderr = child.communicate(timeout=35)
        self.assertEqual(child.returncode, 0, stderr)
        return json.loads(stdout)

    def call(self, operation, **kwargs):
        return self.finish(self.spawn(operation, **kwargs))

    def success(self, operation, **kwargs):
        result = self.call(operation, **kwargs)
        self.assertTrue(result['ok'], result)
        return result['value']

    def check_disk(self, receipt, active=True):
        if active:
            pointer = self.root / 'deployments' / receipt['tenant'] / receipt['environment'] / 'current.json'
            self.assertEqual(pointer.read_bytes(), raw(receipt))
        manifest = (self.root / 'manifests' / (receipt['manifest_sha256'] + '.json')).read_bytes()
        self.assertEqual(hashlib.sha256(manifest).hexdigest(), receipt['manifest_sha256'])
        for item in json.loads(manifest)['packages']:
            body = (self.root / 'objects' / item['sha256']).read_bytes()
            self.assertEqual(hashlib.sha256(body).hexdigest(), item['sha256'])
        with closing(sqlite3.connect(self.root / 'repo.sqlite')) as connection:
            rows = connection.execute('SELECT generation,manifest_sha256 FROM receipts WHERE tenant=? AND environment=? AND key=?',
                                      (receipt['tenant'], receipt['environment'], receipt['key'])).fetchall()
        self.assertEqual(rows, [(receipt['generation'], receipt['manifest_sha256'])])

    def worker(self, number, jobs):
        job_path, result = self.work / ('jobs-%s.jsonl' % number), self.work / ('result-%s.jsonl' % number)
        job_path.write_bytes(b''.join(raw(job) + b'\n' for job in jobs))
        child = subprocess.Popen([sys.executable, '-m', 'app.worker', '--root', str(self.root),
                                  '--catalog', str(self.catalog_path), '--jobs', str(job_path),
                                  '--result', str(result)], cwd=self.work,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.children.append(child)
        return child, result


class FixtureTransactions(FixtureHarness):
    def test_old_replay_after_new_commit_preserves_active_generation(self):
        first = self.success('install', **self.request())
        second = self.success('install', **self.request('second', 2, expected_generation=1))
        replay = self.success('install', **self.request())
        self.assertEqual(replay, first)
        self.check_disk(first, active=False)
        self.check_disk(second)
        self.assertEqual(self.success('active', tenant='tenant-a', environment='prod'), second)

    def test_historical_replay_heals_missing_pointer_to_current(self):
        first = self.success('install', **self.request())
        second = self.success('install', **self.request('second', 2))
        (self.root / 'deployments/tenant-a/prod/current.json').unlink()
        self.assertEqual(self.success('install', **self.request()), first)
        self.check_disk(second)

    def test_real_process_crashes_reopen_and_retry_once(self):
        first = self.success('install', **self.request())
        for hook in ('before_pointer', 'after_pointer'):
            generation = 2 if hook == 'before_pointer' else 3
            request = self.request(hook.replace('_', '-'), 2, expected_generation=generation - 1)
            child = self.spawn('install', **request, crash_at=hook)
            child.communicate(timeout=30)
            self.assertEqual(child.returncode, 71)
            self.success('recover')
            recovered = self.success('active', tenant='tenant-a', environment='prod')
            self.assertEqual(recovered['generation'], generation - 1 if hook == 'before_pointer' else generation)
            self.check_disk(recovered)
            final = self.success('install', **request)
            self.assertEqual(final['generation'], generation)
            self.check_disk(final)
            self.assertEqual(self.success('install', **request), final)
        self.check_disk(first, active=False)
        with closing(sqlite3.connect(self.root / 'repo.sqlite')) as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM receipts').fetchone()[0], 3)

    def test_next_mutation_settles_crash_without_explicit_recover(self):
        self.success('install', **self.request())
        child = self.spawn('install', **self.request('crashed', 2), crash_at='after_pointer')
        child.communicate(timeout=30)
        self.assertEqual(child.returncode, 71)
        third = self.success('install', **self.request('third', 3, expected_generation=2))
        self.assertEqual(third['generation'], 3)
        self.check_disk(third)
        second = self.success('receipt', tenant='tenant-a', environment='prod', key='crashed')
        self.check_disk(second, active=False)

    def test_four_workers_full_request_echo_one_cas_winner(self):
        self.success('install', **self.request())
        requests = [self.request('racer-%d' % i, 2, expected_generation=1) for i in range(4)]
        children = [self.worker(i, [request]) for i, request in enumerate(requests)]
        rows = []
        for (child, path), request in zip(children, requests):
            _, stderr = child.communicate(timeout=35)
            self.assertEqual(child.returncode, 0, stderr)
            result = [json.loads(line) for line in path.read_bytes().splitlines()]
            self.assertEqual(len(result), 1)
            self.assertEqual(result[0]['request'], request)
            rows.extend(result)
        winners = [row for row in rows if row['ok']]
        self.assertEqual(len(winners), 1, rows)
        self.check_disk(winners[0]['receipt'])
        self.assertEqual(winners[0]['receipt']['generation'], 2)

    def test_four_worker_contention_generations_and_gc(self):
        # Shared scope is intentionally harsher than tenant-isolated workers.
        children = [self.worker(i, [self.request('worker-%d-job-%d' % (i, j), 1 + j % 3)
                                   for j in range(12)]) for i in range(4)]
        for _ in range(5):
            self.success('gc')
        rows = []
        for child, path in children:
            _, stderr = child.communicate(timeout=35)
            self.assertEqual(child.returncode, 0, stderr)
            result = [json.loads(line) for line in path.read_bytes().splitlines()]
            self.assertEqual(len(result), 12)
            self.assertTrue(all(row['ok'] for row in result), result)
            rows.extend(result)
        receipts = [row['receipt'] for row in rows]
        self.assertEqual(sorted(row['generation'] for row in receipts), list(range(1, 49)))
        for receipt in receipts:
            self.check_disk(receipt, active=False)
        self.check_disk(max(receipts, key=lambda row: row['generation']))

    def test_gc_defers_on_unreadable_committed_root(self):
        receipt = self.success('install', **self.request())
        manifest = self.root / 'manifests' / (receipt['manifest_sha256'] + '.json')
        manifest.write_bytes(b'corrupt')
        orphan = self.root / 'objects' / ('f' * 64)
        orphan.write_bytes(b'preserve when root unavailable')
        result = self.success('gc')
        self.assertTrue(result['deferred'], result)
        self.assertTrue(orphan.exists())

    def test_malformed_journal_fences_install_and_gc(self):
        first = self.success('install', **self.request())
        (self.root / 'journal/broken.json').write_text('{', encoding='utf-8')
        result = self.call('install', **self.request('next', 2))
        self.assertFalse(result['ok'], result)
        self.assertTrue(self.success('gc')['deferred'])
        self.check_disk(first)

    def test_bool_generation_and_newline_identity_rejected(self):
        first = self.success('install', **self.request())
        for override in [dict(expected_generation=True), dict(key='bad\n')]:
            request = self.request('next', 2)
            request.update(override)
            result = self.call('install', **request)
            self.assertFalse(result['ok'], result)
            self.assertEqual(result['error_type'], 'ValueError')
        self.check_disk(first)

    def test_worker_records_invalid_job_then_continues(self):
        valid = self.request()
        child, result = self.worker('malformed', [[], valid])
        _, stderr = child.communicate(timeout=35)
        self.assertEqual(child.returncode, 0, stderr)
        rows = [json.loads(line) for line in result.read_bytes().splitlines()]
        self.assertEqual([row['ok'] for row in rows], [False, True])
        self.assertEqual(rows[0]['request'], [])
        self.assertEqual(rows[1]['request'], valid)
        self.check_disk(rows[1]['receipt'])


if __name__ == '__main__':
    unittest.main()
