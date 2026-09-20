"""Independent v4 snapshot oracle. Never imports candidate application helpers."""
from __future__ import annotations

import argparse
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import sqlite3
import stat
import struct
import subprocess
import sys
import tempfile
import time
import warnings
import zipfile

NAME = re.compile(r'[a-z][a-z0-9-]{0,63}\Z')
DIGEST = re.compile(r'[0-9a-f]{64}\Z')
RECEIPT = {'tenant', 'environment', 'key', 'generation', 'manifest_sha256'}
SCHEMA = '''
CREATE TABLE receipts(tenant TEXT NOT NULL,environment TEXT NOT NULL,key TEXT NOT NULL,
 generation INTEGER NOT NULL,manifest_sha256 TEXT NOT NULL,request_json TEXT NOT NULL,
 PRIMARY KEY(tenant,environment,key));
CREATE TABLE current(tenant TEXT NOT NULL,environment TEXT NOT NULL,receipt_json TEXT NOT NULL,
 PRIMARY KEY(tenant,environment));
CREATE TABLE journal(tenant TEXT NOT NULL,environment TEXT NOT NULL,key TEXT NOT NULL,
 status TEXT NOT NULL,old_generation INTEGER,old_manifest TEXT,old_receipt TEXT,
 new_generation INTEGER,new_manifest TEXT,new_receipt TEXT NOT NULL,
 PRIMARY KEY(tenant,environment,key));
CREATE TABLE outbox(tenant TEXT NOT NULL,environment TEXT NOT NULL,key TEXT NOT NULL,
 body TEXT NOT NULL,status TEXT NOT NULL,ack TEXT,url TEXT,PRIMARY KEY(tenant,environment,key));
PRAGMA user_version=2;
'''


def raw(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(',', ':'), allow_nan=False).encode('utf-8')


def sha(data):
    return hashlib.sha256(data).hexdigest()


def put(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def write_json(path, value):
    put(path, json.dumps(value, ensure_ascii=False, indent=2).encode('utf-8') + b'\n')


def bytes_map(root):
    """Include empty directories and link identity, without following links."""
    root = Path(root)
    if not root.exists():
        return {}
    result = {}
    for base, dirs, files in os.walk(root, followlinks=False):
        for name in sorted(dirs + files):
            path = Path(base) / name
            info = path.lstat()
            rel = path.relative_to(root).as_posix()
            if stat.S_ISLNK(info.st_mode) or getattr(info, 'st_file_attributes', 0) & 0x400:
                result[rel] = ('link', os.readlink(path))
                if name in dirs:
                    dirs.remove(name)
            elif path.is_dir():
                result[rel] = ('directory',)
            else:
                result[rel] = ('file', sha(path.read_bytes()))
    return result


def fixture(root):
    """Construct real SQLite authority independently of app.Repository."""
    root = Path(root)
    root.mkdir(parents=True, exist_ok=False)
    members = {}
    blobs = [b'common payload\n', b'old payload\x00\n', 'new payload 数据\n'.encode()]
    for body in blobs:
        digest = sha(body)
        members['objects/' + digest] = body
    rows, current = [], []
    for tenant, environment, count in [('alpha', 'prod', 2), ('beta', 'dev', 1), ('beta', 'prod', 2)]:
        for generation in range(1, count + 1):
            manifest = dict(tenant=tenant, environment=environment, packages=[
                dict(name='base', version=1, sha256=sha(blobs[0])),
                dict(name='feature', version=generation, sha256=sha(blobs[generation])),
            ])
            body = raw(manifest)
            digest = sha(body)
            members['manifests/' + digest + '.json'] = body
            receipt = dict(tenant=tenant, environment=environment, key='release-' + str(generation),
                           generation=generation, manifest_sha256=digest)
            request = raw(dict(requirements={'feature': dict(min=generation, max=generation + 1)},
                               expected_generation=generation - 1)).decode()
            rows.append(dict(receipt, request_json=request))
            if generation == count:
                current.append(receipt)
                put(root / 'deployments' / tenant / environment / 'current.json', raw(receipt))
    rows.sort(key=lambda x: (x['tenant'], x['environment'], x['key']))
    current.sort(key=lambda x: (x['tenant'], x['environment']))
    descriptor = dict(format='package-snapshot-v1', schema_version=2, receipts=rows, current=current)
    for name, data in members.items():
        put(root / name, data)
    with sqlite3.connect(root / 'repo.sqlite') as db:
        db.executescript(SCHEMA)
        db.executemany('INSERT INTO receipts VALUES(?,?,?,?,?,?)', [
            tuple(row[x] for x in ('tenant', 'environment', 'key', 'generation', 'manifest_sha256', 'request_json'))
            for row in rows])
        db.executemany('INSERT INTO current VALUES(?,?,?)', [
            (row['tenant'], row['environment'], raw(row).decode()) for row in current])
    db.close()
    members['snapshot.json'] = raw(descriptor)
    return members


def summary(members):
    descriptor = json.loads(members['snapshot.json'])
    return dict(snapshot_id=sha(members['snapshot.json']), receipts=len(descriptor['receipts']),
                current=len(descriptor['current']), manifests=sum(n.startswith('manifests/') for n in members),
                objects=sum(n.startswith('objects/') for n in members))


def zip_bytes(members, *, additions=(), compression=zipfile.ZIP_STORED):
    target = io.BytesIO()
    with warnings.catch_warnings():
        warnings.simplefilter('ignore', UserWarning)
        with zipfile.ZipFile(target, 'w', compression=compression, allowZip64=False) as archive:
            for name, data in sorted(members.items()) + list(additions):
                info = zipfile.ZipInfo(name, (1980, 1, 1, 0, 0, 0))
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                info.compress_type = compression
                archive.writestr(info, data)
    return target.getvalue()


PUBLIC_TEST = '''import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from app.snapshot import export_snapshot, inspect_snapshot, restore_snapshot


class SnapshotPublic(unittest.TestCase):
    def setUp(self):
        self.work = Path(__file__).resolve().parents[1]
        self.source = self.work / 'fixtures' / 'snapshot-source#one'
        self.expected = json.loads((self.work / 'fixtures' / 'snapshot-expected.json').read_bytes())
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_deterministic_export_and_inspect(self):
        a, b = self.root / 'one.zip', self.root / 'two.zip'
        self.assertEqual(export_snapshot(self.source, a), self.expected)
        self.assertEqual(export_snapshot(self.source, b), self.expected)
        self.assertEqual(a.read_bytes(), b.read_bytes())
        self.assertEqual(inspect_snapshot(a), self.expected)

    def test_restore_and_exact_idempotency(self):
        archive, dest = self.root / 'snapshot.zip', self.root / 'restored'
        export_snapshot(self.source, archive)
        self.assertEqual(restore_snapshot(archive, dest), self.expected)
        before = {p.relative_to(dest).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
                  for p in dest.rglob('*') if p.is_file()}
        self.assertEqual(restore_snapshot(archive, dest), self.expected)
        after = {p.relative_to(dest).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
                 for p in dest.rglob('*') if p.is_file()}
        self.assertEqual(before, after)
'''


def seed(work):
    """Add protected public fixtures/tests to an existing fresh workspace."""
    work = Path(work).resolve()
    source = work / 'fixtures' / 'snapshot-source#one'
    expected_file = work / 'fixtures' / 'snapshot-expected.json'
    test_file = work / 'tests' / 'test_snapshot_public.py'
    if source.exists() or expected_file.exists() or test_file.exists():
        raise FileExistsError('v4 seed refuses an existing public snapshot fixture')
    members = fixture(source)
    write_json(expected_file, summary(members))
    put(test_file, PUBLIC_TEST.encode('utf-8'))
    return dict(source='fixtures/snapshot-source#one', expected=summary(members),
                public_test='tests/test_snapshot_public.py')


API_DRIVER = '''import importlib,json,sys
sys.path.insert(0,sys.argv[1])
module=importlib.import_module("app.snapshot")
request=json.loads(sys.stdin.read())
try:
 value=getattr(module,request["op"])(**request["kwargs"])
 print(json.dumps({"ok":True,"value":value}))
except Exception as error:
 print(json.dumps({"ok":False,"error_type":type(error).__name__,"error":str(error),
                   "expected_error":isinstance(error,(ValueError,OSError))}))
'''


def invoke_candidate(work, op, **kwargs):
    # Candidate helpers never define this oracle's expected state.
    command = [sys.executable, '-B', '-c', API_DRIVER, str(work)]
    result = subprocess.run(command, input=json.dumps(dict(op=op, kwargs=kwargs)),
                            cwd=work, capture_output=True, text=True, timeout=30)
    assert result.returncode == 0, (result.returncode, result.stderr[-1500:])
    value = json.loads(result.stdout)
    assert type(value.get('ok')) is bool, value
    return value


def check_restored(root, members):
    root = Path(root)
    root_stat = root.lstat()
    assert stat.S_ISDIR(root_stat.st_mode) and not getattr(root_stat, 'st_file_attributes', 0) & 0x400
    state = bytes_map(root)
    assert not any(value[0] == 'link' for value in state.values()), 'restored links are unsafe'
    descriptor = json.loads(members['snapshot.json'])
    expected_files = {'repo.sqlite'} | (set(members) - {'snapshot.json'})
    for receipt in descriptor['current']:
        name = 'deployments/{tenant}/{environment}/current.json'.format(**receipt)
        expected_files.add(name)
        assert (root/name).read_bytes() == raw(receipt), name
    actual_files = {name for name, value in state.items() if value[0] == 'file'}
    assert actual_files == expected_files, (actual_files-expected_files, expected_files-actual_files)
    expected_dirs = {str(parent).replace('\\', '/') for name in expected_files
                     for parent in Path(name).parents if str(parent) != '.'}
    actual_dirs = {name for name, value in state.items() if value[0] == 'directory'}
    assert actual_dirs <= expected_dirs | {'journal'}, actual_dirs-expected_dirs
    for name, content in members.items():
        if name != 'snapshot.json':
            assert (root/name).read_bytes() == content, name
    conn = sqlite3.connect((root/'repo.sqlite').resolve().as_uri()+'?mode=ro&immutable=1', uri=True)
    try:
        assert conn.execute('PRAGMA user_version').fetchone()[0] == 2
        names = ['tenant','environment','key','generation','manifest_sha256','request_json']
        receipts = [dict(zip(names,row)) for row in conn.execute(
            'SELECT tenant,environment,key,generation,manifest_sha256,request_json FROM receipts ORDER BY tenant,environment,key')]
        assert receipts == descriptor['receipts']
        current = conn.execute('SELECT tenant,environment,receipt_json FROM current ORDER BY tenant,environment').fetchall()
        expected_current = [(row['tenant'], row['environment'], raw(row).decode('utf-8'))
                            for row in descriptor['current']]
        assert current == expected_current, 'current SQL scope/canonical receipt differs from the snapshot'
        assert conn.execute('SELECT COUNT(*) FROM journal').fetchone()[0] == 0
        assert conn.execute('SELECT COUNT(*) FROM outbox').fetchone()[0] == 0
    finally:
        conn.close()


def check_archive(path, members):
    """Check declared ZIP metadata and contents, not unspecified writer versions."""
    path = Path(path)
    assert path.is_file() and not path.is_symlink(), 'export did not create an archive file'
    assert path.stat().st_size <= 9*1024*1024
    encoded = path.read_bytes()
    end = struct.unpack('<4s4H2IH', encoded[-22:])
    assert end[0] == b'PK\x05\x06' and end[1:3] == (0, 0) and end[-1] == 0
    assert end[3] == end[4] == len(members) and end[5] + end[6] == len(encoded)-22, 'ZIP64 or extra end records'
    with zipfile.ZipFile(path) as archive:
        assert not archive.comment
        entries = archive.infolist()
        assert len(entries) <= 256
        assert [info.filename for info in entries] == sorted(members), 'archive member set/order differs'
        assert sum(info.file_size for info in entries) <= 8*1024*1024
        for info in entries:
            assert info.compress_type == zipfile.ZIP_STORED
            assert info.date_time == (1980, 1, 1, 0, 0, 0)
            assert info.create_system == 3 and info.external_attr == 0o100644 << 16
            assert not info.extra and not info.comment and not info.flag_bits & 1
            assert info.file_size <= 2*1024*1024
            assert info.file_size == info.compress_size == len(members[info.filename])
            header = struct.unpack('<4s5H3I2H', encoded[info.header_offset:info.header_offset+30])
            assert header[0] == b'PK\x03\x04' and header[-1] == 0, 'local ZIP extra field'
            assert not header[2] & 1 and header[3] == zipfile.ZIP_STORED
            assert header[4:6] == (0, 33), 'local ZIP timestamp differs'
            assert header[7] != 0xffffffff and header[8] != 0xffffffff, 'ZIP64 local member'
            start = info.header_offset+30
            assert encoded[start:start+header[-2]] == info.filename.encode('utf-8')
            assert archive.read(info) == members[info.filename], info.filename


def check_only_created(folder, before, names):
    after = bytes_map(folder)
    def allowed(name):
        return any(name == item or name.startswith(item + '/') for item in names)
    assert {k:v for k,v in after.items() if not allowed(k)} == before, 'operation changed input or unrelated siblings'


def fixture_case(folder):
    root = folder/'source#percent% unicode-中'
    members = fixture(root)
    archive = folder/'reference.zip'
    archive.write_bytes(zip_bytes(members))
    return root, members, archive


def check_crash_contract(work, folder):
    _root, members, archive = fixture_case(folder)
    destination = folder/'restore'
    unknown = folder/'unknown-stage';unknown.mkdir();(unknown/'sentinel').write_bytes(b'preserve')
    before = bytes_map(folder)
    initial_names = {path.name for path in folder.iterdir()}
    command = [sys.executable,'-B','-m','app.snapshot','restore','--archive',str(archive),
               '--destination',str(destination),'--crash-at','before_publish']
    process = subprocess.run(command, cwd=work, capture_output=True, text=True, timeout=30)
    assert process.returncode == 73, (process.returncode,process.stdout,process.stderr)
    assert not destination.exists()
    created = [path for path in folder.iterdir() if path.name not in initial_names]
    assert len(created) == 1 and created[0].is_dir(), 'exit73 did not leave one complete sibling stage'
    stage = created[0]
    check_restored(stage, members)
    check_only_created(folder, before, [stage.name])
    crashed = bytes_map(folder)
    value = invoke_candidate(work,'restore_snapshot',archive=str(archive),destination=str(destination))
    assert value['ok'] and value.get('value') == summary(members), value
    check_restored(destination,members)
    check_only_created(folder, crashed, [destination.name])
    return dict(real_exit=73, complete_stage_verified=True, abandoned_stage_unchanged=True, retry_verified=True)


def check_cli_contract(work, folder):
    root,members,archive = fixture_case(folder)
    expected = summary(members);output = folder/'export.zip';dest = folder/'restored'
    commands = [(['export','--root',str(root),'--archive',str(output)], [output.name]),
                (['inspect','--archive',str(output)], []),
                (['restore','--archive',str(output),'--destination',str(dest)], [dest.name])]
    for args, created in commands:
        before = bytes_map(folder)
        result = subprocess.run([sys.executable,'-B','-m','app.snapshot',*args],cwd=work,capture_output=True,text=True,timeout=30)
        assert result.returncode == 0 and json.loads(result.stdout) == dict(ok=True,**expected), (result.returncode,result.stdout,result.stderr)
        assert not result.stderr, result.stderr
        if args[0] == 'export':check_archive(output, members)
        if args[0] == 'restore':check_restored(dest, members)
        check_only_created(folder, before, created)
    archive.write_bytes(b'bad zip');before = bytes_map(folder)
    result = subprocess.run([sys.executable,'-B','-m','app.snapshot','inspect','--archive',str(archive)],cwd=work,capture_output=True,text=True,timeout=30)
    value = json.loads(result.stdout)
    assert result.returncode == 1 and value.get('ok') is False and isinstance(value.get('error'),str)
    assert set(value) == {'ok','error'} and not result.stderr and bytes_map(folder) == before
    return dict(cli_operations=4, exported_archive_verified=True, restored_sqlite_and_files_verified=True)


def case_directory(out, index):
    # Human-readable case names belong in receipts. Using them as physical
    # directories put valid fixtures beyond Windows' ordinary path limit.
    folder = Path(out) / ('c%03d' % index)
    folder.mkdir()
    return folder


def input_identity(work):
    work = Path(work)
    files = {p.relative_to(work).as_posix(): sha(p.read_bytes())
             for folder in ('app', 'fixtures', 'tests') for p in (work/folder).rglob('*')
             if p.is_file() and '__pycache__' not in p.parts}
    if (work/'SPEC.md').is_file():
        files['SPEC.md'] = sha((work/'SPEC.md').read_bytes())
    return files


def verify(work, out):
    work, out = Path(work).resolve(), Path(out).resolve()
    out.mkdir(parents=True, exist_ok=False)
    source = work/'app/snapshot.py'
    if not source.is_file():
        report = dict(status='FAIL', results=[dict(case='candidate_available', status='FAIL',
                       error='app/snapshot.py is absent; no functional case was executed')], provider_paid_calls=0)
        write_json(out/'receipt.json', report)
        return report
    source_hash = sha(source.read_bytes())
    original_inputs = input_identity(work)
    oracle_hash = sha(Path(__file__).read_bytes())
    results = []

    def case(name, action):
        folder = case_directory(out, len(results)+1)
        started = time.monotonic()
        try:
            detail = action(folder)
            row = dict(case=name, status='PASS', detail=detail)
        except Exception as error:
            row = dict(case=name, status='FAIL', error_type=type(error).__name__, error=str(error)[:3000])
        row['elapsed_seconds'] = round(time.monotonic()-started, 3)
        row['artifact_directory'] = folder.name
        results.append(row)
        write_json(out/'partial.json', results)

    def success(value, expected):
        assert value['ok'] and value.get('value') == expected, value

    def rejected(value):
        assert not value['ok'] and value.get('expected_error'), value

    base = fixture_case

    def positive(folder):
        root, members, archive = base(folder)
        one, two = folder/'one.zip', folder/'two.zip'
        expected = summary(members)
        for output in (one, two):
            before = bytes_map(folder)
            success(invoke_candidate(work, 'export_snapshot', root=str(root), archive=str(output)), expected)
            check_archive(output, members)
            check_only_created(folder, before, [output.name])
        assert one.read_bytes() == two.read_bytes()
        before = bytes_map(folder)
        success(invoke_candidate(work, 'inspect_snapshot', archive=str(archive)), expected)
        assert bytes_map(folder) == before
        destination = folder/'restored'
        success(invoke_candidate(work, 'restore_snapshot', archive=str(archive), destination=str(destination)), expected)
        check_restored(destination, members)
        check_only_created(folder, before, [destination.name])
        installed = bytes_map(folder)
        success(invoke_candidate(work, 'restore_snapshot', archive=str(archive), destination=str(destination)), expected)
        assert bytes_map(folder) == installed
        return expected
    case('deterministic_export_inspect_restore_and_idempotency', positive)

    case('real_exit73_before_publish_then_retry', lambda folder:check_crash_contract(work,folder))

    def source_reject(folder, mutation):
        root,members,archive=base(folder);mutation(root,members)
        before=bytes_map(folder);target=folder/'new.zip'
        rejected(invoke_candidate(work,'export_snapshot',root=str(root),archive=str(target)))
        assert bytes_map(folder)==before and not target.exists()
        return dict(source_unchanged=True)
    def sql(root,query):
        with sqlite3.connect(root/'repo.sqlite') as db:db.execute(query)
        db.close()
    source_mutations = {
        'source_wal_refused':lambda r,m:put(r/'repo.sqlite-wal',b'unsettled'),
        'source_future_schema_refused':lambda r,m:sql(r,'PRAGMA user_version=99'),
        'source_outbox_obligation_refused':lambda r,m:sql(r,"INSERT INTO outbox VALUES('alpha','prod','pending','{}','pending',NULL,'http://127.0.0.1')"),
        'source_disk_journal_refused':lambda r,m:put(r/'journal/pending.json',b'{}'),
        'source_corrupt_blob_refused':lambda r,m:put(r/next(n for n in m if n.startswith('objects/')),b'corrupt'),
        'source_missing_manifest_refused':lambda r,m:(r/next(n for n in m if n.startswith('manifests/'))).unlink(),
        'source_stale_pointer_refused':lambda r,m:put(r/'deployments/alpha/prod/current.json',raw({
            k:v for k,v in json.loads(m['snapshot.json'])['receipts'][0].items() if k != 'request_json'})),
        'source_extra_root_file_refused':lambda r,m:put(r/'unrecognized',b'keep'),
    }
    for name,mutation in source_mutations.items():
        case(name,lambda folder,mutation=mutation:source_reject(folder,mutation))

    def archive_reject(folder, mutation):
        _root,members,archive=base(folder)
        archive.write_bytes(mutation(members))
        original=archive.read_bytes();destination=folder/'destination';before=bytes_map(folder)
        rejected(invoke_candidate(work,'inspect_snapshot',archive=str(archive)))
        rejected(invoke_candidate(work,'restore_snapshot',archive=str(archive),destination=str(destination)))
        assert archive.read_bytes()==original and not destination.exists() and bytes_map(folder)==before
        return dict(archive_unchanged=True,destination_absent=True)
    def replace_descriptor(members,change):
        changed=dict(members);d=json.loads(changed['snapshot.json']);change(d)
        changed['snapshot.json']=raw(d);return zip_bytes(changed)
    def link_member(members):
        output=io.BytesIO()
        with zipfile.ZipFile(io.BytesIO(zip_bytes(members))) as original,zipfile.ZipFile(output,'w') as changed:
            for info in original.infolist():
                if info.filename.startswith('objects/'):
                    info.external_attr=0o120777 << 16
                changed.writestr(info,original.read(info.filename))
        return output.getvalue()
    archive_mutations = {
        'archive_traversal_refused':lambda m:zip_bytes(m,additions=[('../escape',b'bad')]),
        'archive_duplicate_refused':lambda m:zip_bytes(m,additions=[('snapshot.json',m['snapshot.json'])]),
        'archive_extra_member_refused':lambda m:zip_bytes(m,additions=[('unexpected',b'bad')]),
        'archive_compression_refused':lambda m:zip_bytes(m,compression=zipfile.ZIP_DEFLATED),
        'archive_missing_object_refused':lambda m:zip_bytes({n:b for n,b in m.items() if n!=next(x for x in m if x.startswith('objects/'))}),
        'archive_corrupt_object_refused':lambda m:zip_bytes(dict(m,**{next(x for x in m if x.startswith('objects/')):b'bad'})),
        'archive_noncanonical_json_refused':lambda m:zip_bytes(dict(m,**{'snapshot.json':m['snapshot.json']+b'\n'})),
        'archive_boolean_generation_refused':lambda m:replace_descriptor(m,lambda d:d['receipts'][0].update(generation=True)),
        'archive_wrong_current_refused':lambda m:replace_descriptor(m,lambda d:d['current'][0].update(manifest_sha256='0'*64)),
        'archive_over_member_cap_with_extra_names_refused':lambda m:zip_bytes(m,additions=[('extra-%03d'%i,b'x') for i in range(256)]),
        'archive_oversized_object_with_wrong_digest_refused':lambda m:zip_bytes(dict(m,**{next(x for x in m if x.startswith('objects/')):b'x'*(2*1024*1024+1)})),
        'archive_link_member_refused':link_member,
        'archive_duplicate_json_key_refused':lambda m:zip_bytes(dict(m,**{'snapshot.json':b'{"format":"wrong",'+m['snapshot.json'][1:]})),
    }
    for name,mutation in archive_mutations.items():
        case(name,lambda folder,mutation=mutation:archive_reject(folder,mutation))

    def target_conflict(folder, corrupt):
        _root,members,archive=base(folder);destination=folder/'existing'
        if corrupt:
            fixture(destination)
            put(destination/next(n for n in members if n.startswith('objects/')),b'corrupt')
        else:destination.mkdir()
        before=bytes_map(folder)
        rejected(invoke_candidate(work,'restore_snapshot',archive=str(archive),destination=str(destination)))
        assert bytes_map(folder)==before
        return dict(destination_unchanged=True)
    case('existing_empty_destination_conflict',lambda f:target_conflict(f,False))
    case('existing_corrupt_destination_conflict',lambda f:target_conflict(f,True))
    case('cli_single_document_success_and_failure',lambda folder:check_cli_contract(work,folder))
    assert sha(source.read_bytes())==source_hash,'candidate source changed during independent verification'
    assert input_identity(work)==original_inputs,'candidate workspace inputs changed during independent verification'
    assert sha(Path(__file__).read_bytes())==oracle_hash,'oracle changed during independent verification'
    report=dict(status='PASS' if all(r['status']=='PASS' for r in results) else 'FAIL',results=results,
                candidate_sha256=source_hash,provider_paid_calls=0,
                source_identity=original_inputs,oracle_sha256=oracle_hash,
                coverage='bounded named cases; not every invalid ZIP/filesystem mutation')
    write_json(out/'receipt.json',report)
    return report


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--work', type=Path, required=True)
    parser.add_argument('--out', type=Path)
    parser.add_argument('--seed', action='store_true')
    args = parser.parse_args()
    if args.seed:
        print(json.dumps(seed(args.work), ensure_ascii=False))
        return 0
    if args.out is None:
        parser.error('--out is required for verification')
    result = verify(args.work, args.out)
    print(json.dumps(result, ensure_ascii=False))
    return 0 if result['status'] == 'PASS' else 1


if __name__ == '__main__':
    raise SystemExit(main())
