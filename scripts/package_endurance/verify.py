"""Independent assertions on actual disk/SQLite, no candidate oracle imports."""
import argparse
import hashlib
import itertools
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent


def raw(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def reference_resolve(catalog, roots):
    """Exhaustive small-graph oracle, not the candidate backtracking solver."""
    names = sorted(catalog)
    assert len(names) <= 7, 'bounded oracle universe'
    solutions = []
    for choices in itertools.product(*[catalog[name] for name in names]):
        selected = dict(zip(names, choices))
        active, visiting = set(), set()
        def visit(name, constraint):
            if name not in selected: return False
            row = selected[name]
            if not constraint['min'] <= row['version'] < constraint['max']: return False
            if name in visiting: return False
            if name in active: return True
            visiting.add(name)
            if not all(visit(dep, bound) for dep, bound in row['deps'].items()): return False
            visiting.remove(name)
            active.add(name)
            return True
        if all(visit(name, bound) for name, bound in roots.items()):
            solutions.append({name: selected[name]['version'] for name in sorted(active)})
    if not solutions: raise ValueError('unsatisfiable')
    return max(solutions, key=lambda row: tuple(row.items()))


def verify_disk(root, catalog, receipt, active=True):
    """Read actual pointer, manifest, objects, DB receipt; never recompute app output."""
    root = Path(root)
    pointer = root / 'deployments' / receipt['tenant'] / receipt['environment'] / 'current.json'
    assert set(receipt)=={'tenant','environment','key','generation','manifest_sha256'},'receipt shape'
    if active:
        actual_receipt = json.loads(pointer.read_bytes())
        assert actual_receipt == receipt, ('pointer mismatch', actual_receipt, receipt)
        assert pointer.read_bytes()==raw(receipt),'noncanonical pointer'
    digest = receipt['manifest_sha256']
    body = (root / 'manifests' / (digest + '.json')).read_bytes()
    assert sha(body) == digest, 'manifest hash mismatch'
    manifest = json.loads(body)
    assert set(manifest)=={'tenant','environment','packages'},'manifest shape'
    assert raw(manifest) == body, 'noncanonical manifest'
    assert manifest['tenant'] == receipt['tenant'] and manifest['environment'] == receipt['environment']
    assert manifest['packages'] == sorted(manifest['packages'], key=lambda p:p['name'])
    versions = {}
    for package in manifest['packages']:
        assert set(package)=={'name','version','sha256'},'package shape'
        choices = [v for v in catalog[package['name']] if v['version'] == package['version']]
        assert len(choices) == 1 and choices[0]['sha256'] == package['sha256']
        assert sha((root / 'objects' / package['sha256']).read_bytes()) == package['sha256'], 'blob hash mismatch'
        versions[package['name']] = package['version']
    assert len(versions) == len(manifest['packages']), 'duplicate package'
    for package in manifest['packages']:
        row = next(v for v in catalog[package['name']] if v['version'] == package['version'])
        for dep, bound in row['deps'].items():
            assert dep in versions and bound['min'] <= versions[dep] < bound['max'], 'unsatisfied dependency'
    with sqlite3.connect(f'file:{(root / "repo.sqlite").as_posix()}?mode=ro', uri=True) as connection:
        found = connection.execute('SELECT generation,manifest_sha256 FROM receipts WHERE tenant=? AND environment=? AND key=?', (receipt['tenant'], receipt['environment'], receipt['key'])).fetchall()
    connection.close()
    assert found == [(receipt['generation'], digest)], 'durable receipt mismatch'
    return versions


def invoke(work, request, timeout=30):
    result = subprocess.run([sys.executable, str(HERE / 'invoke.py'), str(work)], input=json.dumps(request)+'\n', text=True, encoding='utf-8', capture_output=True, timeout=timeout)
    if result.returncode: raise AssertionError(('candidate process failed', result.returncode, result.stderr[-1000:]))
    return json.loads(result.stdout.strip())


def calibration(campaign):
    root = campaign / 'oracle-calibration'
    root.mkdir(exist_ok=False)
    work = campaign / 'workspace'
    catalog = json.loads((work / 'fixtures/catalog.json').read_text(encoding='utf-8'))
    package = dict(name='pkg-000', version=1, sha256=catalog['pkg-000'][0]['sha256'])
    manifest = dict(tenant='calibration', environment='test', packages=[package])
    body = raw(manifest)
    receipt = dict(tenant='calibration', environment='test', key='first', generation=1, manifest_sha256=sha(body))
    for d in ['objects', 'manifests', 'deployments/calibration/test']:(root/d).mkdir(parents=True, exist_ok=True)
    shutil.copyfile(work/'fixtures/blobs'/package['sha256'], root/'objects'/package['sha256'])
    (root/'manifests'/(sha(body)+'.json')).write_bytes(body)
    pointer = root/'deployments/calibration/test/current.json'
    pointer.write_bytes(raw(receipt))
    c=sqlite3.connect(root/'repo.sqlite')
    c.execute('CREATE TABLE receipts(tenant,environment,key,generation,manifest_sha256)')
    c.execute('INSERT INTO receipts VALUES(?,?,?,?,?)', ('calibration','test','first',1,sha(body)));c.commit();c.close()
    verify_disk(root,catalog,receipt)
    rejected=[]
    for kind in ['blob','pointer','receipt']:
        path=root/'objects'/package['sha256'] if kind=='blob' else pointer
        if kind!='receipt':
            saved=path.read_bytes();path.write_bytes(b'corrupted')
        else:
            c=sqlite3.connect(root/'repo.sqlite');c.execute('DELETE FROM receipts');c.commit();c.close()
        try:verify_disk(root,catalog,receipt)
        except (AssertionError,ValueError):rejected.append(kind)
        else:raise AssertionError(('oracle accepted corrupt fixture',kind))
        if kind!='receipt':path.write_bytes(saved)
    assert rejected==['blob','pointer','receipt']
    return {'status':'PASS','positive_disk_fixture':True,'rejected':rejected}


def acceptance(campaign, label):
    work=campaign/'workspace'; out=campaign/'verification'/label;out.mkdir(parents=True,exist_ok=False)
    catalog=json.loads((work/'fixtures/catalog.json').read_text(encoding='utf-8'))
    results=[]
    def case(name,fn):
        try:detail=fn();results.append(dict(case=name,status='PASS',detail=detail))
        except Exception as exc:results.append(dict(case=name,status='FAIL',error_type=type(exc).__name__,error=str(exc)[:2500]))
    def resolver():
        small={'a':[dict(version=1,sha256='1'*64,deps={'b':dict(min=1,max=2)}),dict(version=2,sha256='2'*64,deps={'b':dict(min=2,max=3)})], 'b':[dict(version=1,sha256='3'*64,deps={}),dict(version=2,sha256='4'*64,deps={})]}
        roots={'a':dict(min=1,max=3),'b':dict(min=1,max=2)}
        got=invoke(work,dict(op='resolve',catalog=small,requirements=roots));assert got['ok'],got
        assert got['value']==reference_resolve(small,roots),got
        return got['value']
    case('backtracking_matches_exhaustive_oracle',resolver)
    def installation():
        root=out/'repository'; request=dict(tenant='tenant-a',environment='prod',key='deploy-one',requirements={'pkg-079':dict(min=3,max=4)})
        result=invoke(work,dict(op='install',root=str(root),kwargs=request),90);assert result['ok'],result
        receipt=result['value']; versions=verify_disk(root,catalog,receipt)
        assert len(versions)==80 and set(versions.values())=={3},len(versions)
        again=invoke(work,dict(op='install',root=str(root),kwargs=request));assert again==result
        conflict=dict(request,requirements={'pkg-079':dict(min=2,max=3)})
        bad=invoke(work,dict(op='install',root=str(root),kwargs=conflict));assert not bad['ok'],bad
        stale=dict(request,key='stale',expected_generation=0)
        bad=invoke(work,dict(op='install',root=str(root),kwargs=stale));assert not bad['ok'],bad
        second=dict(request,key='deploy-two',requirements={'pkg-079':dict(min=4,max=5)},expected_generation=1)
        updated=invoke(work,dict(op='install',root=str(root),kwargs=second));assert updated['ok'],updated
        assert updated['value']['generation']==2,updated
        verify_disk(root,catalog,updated['value'])
        replay=invoke(work,dict(op='install',root=str(root),kwargs=request));assert replay==result,replay
        verify_disk(root,catalog,updated['value'])
        collected=invoke(work,dict(op='gc',root=str(root)));assert collected['ok'],collected
        verify_disk(root,catalog,updated['value'])
        for name,version in versions.items():assert (root/'objects'/catalog[name][version-1]['sha256']).is_file(), 'GC deleted referenced old object'
        return {'packages':80,'generation':2,'idempotency':True,'stale_cas_refused':True,'gc_retains_history':True}
    case('disk_install_replay_conflict_cas_gc',installation)
    def source_corruption():
        # Fresh independent copy: never change immutable campaign source.
        copy=out/'corrupt-work';shutil.copytree(work/'app',copy/'app',ignore=shutil.ignore_patterns('__pycache__'))
        shutil.copytree(work/'fixtures',copy/'fixtures')
        good=invoke(copy,dict(op='install',root=str(out/'good-source-repo'),kwargs=dict(tenant='tenant-a',environment='prod',key='good-source',requirements={'pkg-000':dict(min=1,max=2)})))
        assert good['ok'], ('positive control failed; corruption not exercised', good)
        verify_disk(out/'good-source-repo',catalog,good['value'])
        src=copy/'fixtures/blobs'/catalog['pkg-000'][0]['sha256'];b=src.read_bytes();stamp=src.stat();src.write_bytes(bytes([b[0]^1])+b[1:]);os.utime(src,ns=(stamp.st_atime_ns,stamp.st_mtime_ns))
        result=invoke(copy,dict(op='install',root=str(out/'corrupt-repo'),kwargs=dict(tenant='tenant-a',environment='prod',key='corrupt',requirements={'pkg-000':dict(min=1,max=2)})))
        assert not result['ok'] and result.get('error_type')=='ValueError',('corruption was not explicitly rejected',result)
        return result
    case('corrupted_source_refused',source_corruption)
    report=dict(status='PASS' if all(r['status']=='PASS' for r in results) else 'FAIL',results=results,time=time.time())
    (out/'receipt.json').write_text(json.dumps(report,indent=2),encoding='utf-8');return report


def main():
    ap=argparse.ArgumentParser();ap.add_argument('campaign',type=Path);ap.add_argument('--calibrate',action='store_true');ap.add_argument('--label',default='acceptance');a=ap.parse_args()
    c=a.campaign.resolve()
    result=calibration(c) if a.calibrate else acceptance(c,a.label)
    if a.calibrate:(c/'oracle-calibration.json').write_text(json.dumps(result,indent=2),encoding='utf-8')
    print(json.dumps(result,ensure_ascii=False))
    return 0 if result['status']=='PASS' else 1

if __name__=='__main__':raise SystemExit(main())
