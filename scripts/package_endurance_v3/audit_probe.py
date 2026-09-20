"""Independent acceptance for the bounded real-model authored audit CLI."""
import argparse
import hashlib
import json
import shutil
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

REPO=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(REPO/'scripts'))
sys.path.insert(0,str(REPO/'scripts/package_endurance'))
from runner_process_tree import spawn_windows_owned,stop_windows_owned
from exercise import call,request,success
from common import write_json


def disk_digest(root):
    return {str(p.relative_to(root)):hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob('*') if p.is_file()}


def run(stage,label):
    work=stage/'workspace';out=stage/'audit-verification'/label;out.mkdir(parents=True,exist_ok=False)
    root=out/'good';first=success(call(work,root,'install',**request()))
    last=success(call(work,root,'install',**request(key='second',version=3,expected_generation=1)))
    def blob(p):
        f=next((p/'objects').iterdir());b=f.read_bytes();f.write_bytes(bytes([b[0]^1])+b[1:])
    def manifest(p):(p/'manifests'/(last['manifest_sha256']+'.json')).write_bytes(b'{}')
    def no_receipts(p):
        with sqlite3.connect(p/'repo.sqlite') as db:db.execute('DELETE FROM receipts')
        db.close()
    def pointer(p):
        f=p/'deployments/tenant-a/prod/current.json';v=json.loads(f.read_bytes());v['generation']=999;f.write_text(json.dumps(v))
    def unsafe_digest(p):
        with sqlite3.connect(p/'repo.sqlite') as db:db.execute("UPDATE receipts SET manifest_sha256='../../outside'")
        db.close()
    def wal_sidecar(p):(p/'repo.sqlite-wal').write_bytes(b'pending-wal-data-must-not-be-ignored')
    cases=[('valid_history',None,True),('corrupt_blob',blob,False),('corrupt_manifest',manifest,False),
           ('orphan_pointer_no_receipts',no_receipts,False),('pointer_mismatch',pointer,False),('unsafe_digest',unsafe_digest,False),('needs_quiescent_wal',wal_sidecar,False)]
    results=[]
    for name,mutate,expected in cases:
        candidate=out/name;shutil.copytree(root,candidate)
        if mutate:mutate(candidate)
        before=disk_digest(candidate);child=None;observed={}
        try:
            child=spawn_windows_owned([sys.executable,'-m','app.audit','--root',str(candidate)],cwd=work,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding='utf-8')
            stdout,stderr=child.communicate(timeout=30)
            observed=dict(exit=child.returncode,stdout=stdout[-8000:],stderr=stderr[-1000:])
            value=json.loads(stdout);observed['output']=value
            assert type(value.get('ok')) is bool and isinstance(value.get('errors'),list),value
            assert value['ok']==expected,(name,expected,value)
            assert child.returncode==(0 if expected else 1),observed
            if expected:assert value['receipts']==2 and value['errors']==[],value
            else:assert value['errors'],value
            assert disk_digest(candidate)==before,'auditor mutated its repository'
            verdict='PASS'
        except Exception as error:
            verdict='FAIL';observed.update(error_type=type(error).__name__,error=str(error))
        finally:
            if child is not None:
                cleanup=stop_windows_owned(child,0,10);observed['cleanup']=cleanup
                if not cleanup['tree_confirmed']:verdict='FAIL'
        results.append(dict(case=name,status=verdict,detail=observed))
    report=dict(status='PASS' if all(x['status']=='PASS' for x in results) else 'FAIL',results=results,
                audit_source_sha256=hashlib.sha256((work/'app/audit.py').read_bytes()).hexdigest() if (work/'app/audit.py').exists() else None)
    write_json(out/'receipt.json',report)
    print(json.dumps(dict(status=report['status'],results=[{k:v for k,v in x.items() if k!='detail'}|{'error':x['detail'].get('error')} for x in results]),ensure_ascii=False))
    return report


if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('stage',type=Path);p.add_argument('--label',required=True)
    a=p.parse_args();r=run(a.stage.resolve(),a.label);raise SystemExit(0 if r['status']=='PASS' else 1)
