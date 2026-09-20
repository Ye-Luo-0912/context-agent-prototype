"""Concurrent install/GC load with retained process-tree ownership and real gates."""
import argparse
import hashlib
import json
import os
import queue
import subprocess
import sys
import threading
import time
from pathlib import Path

REPO=Path(__file__).resolve().parents[2]
sys.path.insert(0,str(REPO/'scripts'))
sys.path.insert(0,str(REPO/'scripts/package_endurance'))
from runner_process_tree import WindowsLaunchCleanupError,spawn_windows_owned,stop_windows_owned
from verify import verify_disk
from common import identity,assert_identity,write_json,campaign_limits,oracle_identity


class Worker:
    def __init__(self,work,root,out,label):
        self.label=label;self.rows=[];self.queue=queue.Queue(maxsize=64);self.err=(out/(label+'.stderr')).open('wb')
        cmd=[sys.executable,str(Path(__file__).with_name('load_worker.py')),'--work',str(work),'--root',str(root)]
        self.process=None;self.reader=None
        try:
            self.process=spawn_windows_owned(cmd,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=self.err,text=True,encoding='utf-8')
            self.reader=threading.Thread(target=self._read,daemon=True);self.reader.start()
            ready=self.next(30);assert ready['phase']=='ready',ready
        except BaseException as error:
            launch_failed=isinstance(error,WindowsLaunchCleanupError)
            if launch_failed:self.process=error.process
            cleanup=None
            try:
                if self.process is not None:cleanup=stop_windows_owned(self.process,0,10)
            except BaseException as cleanup_error:
                if not launch_failed:raise
                cleanup=dict(outcome='unconfirmed',tree_confirmed=False,scope='windows_job',error=repr(cleanup_error))
            finally:
                if self.reader is not None:self.reader.join(5)
                for pipe in (() if self.process is None else (self.process.stdin,self.process.stdout)):
                    if pipe is not None:pipe.close()
                self.err.close()
            if launch_failed:
                write_json(out/(label+'.startup-cleanup.json'),dict(launch=error.cleanup,retry=cleanup))
                error.cleanup=dict(error.cleanup,retry=cleanup)
                raise
            if cleanup is not None:assert cleanup['tree_confirmed'],cleanup
            raise
    def _read(self):
        try:
            for line in self.process.stdout:
                self.queue.put(json.loads(line),timeout=5)
        except Exception as error:self.queue.put(dict(phase='reader_error',error=str(error)),timeout=5)
    def next(self,timeout=30):
        try:row=self.queue.get(timeout=timeout)
        except queue.Empty:raise TimeoutError(f'worker {self.label} produced no result; exit={self.process.poll()}')
        self.rows.append(row);return row
    def send(self,message):
        self.process.stdin.write(json.dumps(message)+'\n');self.process.stdin.flush()
    def wait(self,phase,timeout=30):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            row=self.next(max(.001,end-time.monotonic()))
            if row['phase']==phase:return row
            assert row['phase'] in ('entered','result','pointer_gate'),row
            if row['phase']=='result' and phase!='result':raise AssertionError(('missed gate',row))
        raise TimeoutError(phase)
    def close(self):
        if self.process.poll() is None:
            try:self.send(dict(op='stop'));self.process.stdin.close()
            except (OSError,ValueError):pass
            try:self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:pass
        outcome=stop_windows_owned(self.process,2,10)
        self.reader.join(5)
        for pipe in (self.process.stdin,self.process.stdout):
            if pipe is not None:pipe.close()
        self.err.close()
        assert outcome['tree_confirmed'],outcome
        assert not self.reader.is_alive(),'reader still alive after process tree exit'
        return outcome


def run(stage,seconds,interval,label='load',acceptance_label='accepted'):
    work=stage/'workspace';out=stage/label
    gate=json.loads((stage/'verification'/acceptance_label/'receipt.json').read_bytes())
    assert gate['status']=='PASS','independent acceptance failed'
    expected=gate['source_identity'];assert_identity(work,expected)
    assert gate['oracle_identity']==oracle_identity(),'oracle changed after acceptance'
    controller_files=[Path(__file__),Path(__file__).with_name('load_worker.py'),REPO/'scripts/runner_process_tree.py']
    controller_identity={str(p.relative_to(REPO)):hashlib.sha256(p.read_bytes()).hexdigest() for p in controller_files}
    limits=campaign_limits(stage);started=time.time();end=min(started+seconds,limits['deadline_epoch']-30)
    assert end>started,'campaign expired'
    out.mkdir(exist_ok=False)
    root=work/'deployments'/('shared' if label=='load' else label);catalog=json.loads((work/'fixtures/catalog.json').read_bytes())
    workers=[];old_workers=[];gc=None;cleanup=[];crashes=[];overlaps=0;count=0;batches=0;error=None
    def receipt(status):
        value=dict(status=status,source='assisted_application_live_processes',elapsed_seconds=time.time()-started,target_seconds=seconds,
                   batches=batches,verified_receipts=count,overlap_batches=overlaps,worker_crashes=crashes,cleanup=cleanup,error=error,
                   source_identity=expected,provider_paid_calls=0)
        value['controller_identity']=controller_identity
        write_json(out/'receipt.json',value);return value
    try:
        for i in range(4):workers.append(Worker(work,root,out,f'worker-{i}'))
        gc=Worker(work,root,out,'gc')
        while time.time()<end:
            cycle=time.time();messages=[]
            for i in range(4):
                kwargs=dict(tenant=f'load-{i}',environment='prod',key=f'{label}-batch-{batches}',requirements={'pkg-039':dict(min=1+batches%4,max=2+batches%4)})
                message=dict(id=f'install-{batches}-{i}',op='install',kwargs=kwargs)
                if batches in (2,6) and i==0:message['gate']='before' if batches==2 else 'after'
                messages.append(message);workers[i].send(message)
            if batches in (2,6):
                observed=workers[0].wait('pointer_gate')
                gc.send(dict(id=f'gc-{batches}',op='gc'))
                gc_entered=gc.wait('entered')
                assert workers[0].process.poll() is None,'gate process not live'
                # The GC call is outstanding against the same repository while
                # an install holds a real pointer publication boundary.
                time.sleep(.1)
                workers[0].process.kill();workers[0].process.wait(timeout=10)
                crashes.append(dict(batch=batches,pid=workers[0].process.pid,gate=observed,gc_entered=gc_entered,exit=workers[0].process.returncode))
                old_workers.append(workers[0]);cleanup.append(workers[0].close())
                workers[0]=Worker(work,root,out,f'worker-0-restart-{batches}')
                workers[0].send(dict(id=f'recover-{batches}',op='recover'));assert workers[0].wait('result')['ok']
                retry=dict(messages[0]);retry.pop('gate');workers[0].send(retry)
            else:gc.send(dict(id=f'gc-{batches}',op='gc'))
            outcomes=[]
            for i,worker in enumerate(workers):
                row=worker.wait('result');assert row['ok'],row
                assert row['id']==messages[i]['id'],('uncorrelated worker result',row)
                req=messages[i]['kwargs'];receipt_value=row['value']
                assert all(receipt_value[k]==req[k] for k in ('tenant','environment','key')),('wrong receipt identity',row)
                assert receipt_value['generation']==batches+1,('wrong generation',row)
                versions=verify_disk(root,catalog,receipt_value,active=False)
                assert versions=={f'pkg-{j:03}':1+batches%4 for j in range(40)},('wrong selected package versions',versions)
                outcomes.append(row);count+=1
            gc_result=gc.wait('result');assert gc_result['ok'],gc_result
            assert gc_result['id']==f'gc-{batches}',gc_result
            for row in outcomes:verify_disk(root,catalog,row['value'],active=True)
            overlap=any(max(row['started'],gc_result['started'])<min(row['finished'],gc_result['finished']) for row in outcomes)
            if overlap or batches in (2,6):overlaps+=1
            with (out/'timeline.jsonl').open('a',encoding='utf-8') as log:
                log.write(json.dumps(dict(batch=batches,installs=outcomes,gc=gc_result,overlap=overlap))+'\n')
            batches+=1;receipt('RUNNING')
            time.sleep(max(0,min(interval-(time.time()-cycle),end-time.time())))
        assert len(crashes)==2,'two observed pointer-boundary process deaths required'
        assert overlaps>0,'install and GC never overlapped'
        import sqlite3
        with sqlite3.connect(f'file:{(root/"repo.sqlite").as_posix()}?mode=ro',uri=True) as db:
            historical=db.execute('SELECT tenant,environment,key,generation,manifest_sha256 FROM receipts').fetchall()
        db.close()
        for values in historical:
            verify_disk(root,catalog,dict(zip(('tenant','environment','key','generation','manifest_sha256'),values)),active=False)
        assert_identity(work,expected)
        assert controller_identity=={str(p.relative_to(REPO)):hashlib.sha256(p.read_bytes()).hexdigest() for p in controller_files},'load controller changed during run'
        status='PASS' if time.time()-started>=seconds else 'INCOMPLETE_DEADLINE'
    except BaseException as exc:
        error=dict(type=type(exc).__name__,message=str(exc));status='FAIL'
        if isinstance(exc,WindowsLaunchCleanupError):
            cleanup.append(exc.cleanup);status='CLEANUP_UNCONFIRMED'
    finally:
        for worker in workers+([gc] if gc else []):
            try:cleanup.append(worker.close())
            except Exception as exc:
                error=dict(type=type(exc).__name__,message=str(exc));status='CLEANUP_UNCONFIRMED'
    result=receipt(status);print(json.dumps({k:v for k,v in result.items() if k!='source_identity'},ensure_ascii=False));return result


if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('stage',type=Path);p.add_argument('--seconds',type=int,default=1800);p.add_argument('--interval',type=float,default=1);p.add_argument('--label',default='load');p.add_argument('--acceptance-label',default='accepted')
    a=p.parse_args();result=run(a.stage.resolve(),a.seconds,a.interval,a.label,a.acceptance_label);raise SystemExit(0 if result['status']=='PASS' else 1)
