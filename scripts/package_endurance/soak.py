"""Wall-clock deployment load on a frozen candidate; never counts as Runtime load."""
import argparse
import hashlib
import json
import shutil
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from limits import deadline
from exercise import call, request, success
from verify import raw, verify_disk


def run(campaign, seconds, interval):
    # A long run must never turn a failed deployment gate into apparent load
    # coverage. Read the independent receipts before creating any load state.
    for label in ('final-independent', 'final-deployment'):
        gate=campaign/'verification'/label/'receipt.json'
        if not gate.exists() or json.loads(gate.read_text(encoding='utf-8'))['status']!='PASS':
            raise SystemExit('deployment acceptance required before load: '+label)
    out=campaign/'load';out.mkdir(exist_ok=False)
    work=out/'workspace';work.mkdir()
    for name in ['app','fixtures']:
        shutil.copytree(campaign/'workspace'/name,work/name,ignore=shutil.ignore_patterns('__pycache__'))
    catalog=json.loads((work/'fixtures/catalog.json').read_bytes())
    source={str(p.relative_to(work)):hashlib.sha256(p.read_bytes()).hexdigest()
            for p in (work/'app').rglob('*') if p.is_file()}
    root=out/'repository';started=time.time();end=min(started+seconds,deadline(campaign)-30)
    if end<=started:raise RuntimeError('campaign deadline exhausted')
    rows=[];crashes=[];failures=[];batches=0;jobs_ok=0;children=[];handles=[]
    def checkpoint(status):
        report=dict(status=status,source='application_workers_frozen_candidate',
                    started_epoch=started,elapsed_s=round(time.time()-started,2),
                    target_seconds=seconds,deadline_epoch=deadline(campaign),
                    batches=batches,jobs_ok=jobs_ok,worker_crashes=crashes,
                    failures=failures,app_source_sha256=source,provider_paid_calls=0,
                    runtime_endurance_claim=False)
        (out/'receipt.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
        return report
    try:
        while time.time()<end:
            cycle=time.time();batch=out/f'batch-{batches:05}';batch.mkdir();children=[];handles=[]
            for i in range(4):
                kill_this=batches in (2,6) and i==0
                count=200 if kill_this else 3
                jobs=[request(key=f'b-{batches}-w-{i}-j-{j}',version=1+(batches%4),tenant=f'tenant-{i}') for j in range(count)]
                jobfile=batch/f'jobs-{i}.jsonl';result=batch/f'result-{i}.jsonl'
                jobfile.write_bytes(b''.join(raw(job)+b'\n' for job in jobs))
                log=(batch/f'worker-{i}.log').open('wb');handles.append(log)
                p=subprocess.Popen([sys.executable,'-m','app.worker','--root',str(root),'--catalog',str(work/'fixtures/catalog.json'),'--jobs',str(jobfile),'--result',str(result)],cwd=work,stdout=log,stderr=log)
                children.append((p,result,jobs,kill_this))
            for p,result,jobs,kill_this in children:
                if kill_this:
                    wait_end=time.time()+15
                    while time.time()<wait_end and p.poll() is None:
                        if result.exists() and b'\n' in result.read_bytes():break
                        time.sleep(.005)
                    observed=result.exists() and b'\n' in result.read_bytes()
                    assert observed and p.poll() is None,('crash trigger not reached while worker live',p.pid)
                    p.kill();p.wait(timeout=10)
                    crashes.append(dict(pid=p.pid,batch=batches,exit=p.returncode,first_result_observed=True))
                else:p.wait(timeout=30)
                assert result.exists(),('no result',p.pid,p.returncode)
                raw_lines=result.read_bytes().splitlines();parsed=[]
                for line in raw_lines:
                    try:parsed.append(json.loads(line))
                    except ValueError:
                        assert kill_this and line is raw_lines[-1],'unexpected truncated result'
                if not kill_this:assert len(parsed)==len(jobs),(p.pid,len(parsed),len(jobs))
                else:assert 0<len(parsed)<len(jobs),'terminated worker falsely completed remaining jobs'
                for row in parsed:
                    assert row['ok'],row
                    verify_disk(root,catalog,row['receipt'],active=False)
                    jobs_ok+=1
                if not kill_this and parsed:verify_disk(root,catalog,parsed[-1]['receipt'])
                rows.append(dict(batch=batches,pid=p.pid,exit=p.returncode,reported=len(parsed),killed=kill_this))
            for h in handles:h.close()
            handles=[]
            # Reopen through the candidate API after death. Disk verification
            # checks the reconciled active state rather than guessing outcome.
            success(call(work,root,'recover'))
            if batches%10==0:success(call(work,root,'gc'))
            for i in range(4):
                active=success(call(work,root,'active',tenant=f'tenant-{i}',environment='prod'))
                verify_disk(root,catalog,active)
            with (out/'workers.jsonl').open('a',encoding='utf-8') as log:
                for row in rows:log.write(json.dumps(row)+'\n')
            rows=[];batches+=1;checkpoint('RUNNING')
            time.sleep(max(0,min(interval-(time.time()-cycle),end-time.time())))
        elapsed=time.time()-started
        assert len(crashes)==2,'two observed worker deaths required'
        final_source={str(p.relative_to(work)):hashlib.sha256(p.read_bytes()).hexdigest()
                      for p in (work/'app').rglob('*') if p.is_file() and '__pycache__' not in p.parts}
        assert source==final_source,'frozen candidate changed during load'
        report=checkpoint('PASS' if elapsed>=seconds else 'INCOMPLETE_DEADLINE')
    except BaseException as error:
        failures.append(dict(error_type=type(error).__name__,error=str(error)[:2500]))
        report=checkpoint('FAIL')
    finally:
        for p,*_ in children:
            if p.poll() is None:p.kill()
            p.wait(timeout=10)
        for h in handles:h.close()
    print(json.dumps(report,ensure_ascii=False),flush=True)
    return report


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('campaign',type=Path)
    parser.add_argument('--seconds',type=int,default=5400);parser.add_argument('--interval',type=float,default=5)
    args=parser.parse_args();report=run(args.campaign.resolve(),args.seconds,args.interval)
    raise SystemExit(0 if report['status']=='PASS' else 1)
