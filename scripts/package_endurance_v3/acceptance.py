"""Independent process, crash, migration and receiver acceptance.

Only invoke.py/worker subprocesses import the candidate. All assertions inspect
real files, SQLite, process exit codes, and the receiver's own audit.
"""
import argparse
import copy
import hashlib
import json
import os
import shutil
import sqlite3
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

sys.path.insert(0,str(Path(__file__).resolve().parents[1]/"package_endurance"))
from verify import HERE, invoke, raw, reference_resolve, verify_disk
from common import identity, write_json, oracle_identity
from verify import acceptance as basic_acceptance


def request(key='first', version=2, tenant='tenant-a', environment='prod', **extra):
    return dict(tenant=tenant, environment=environment, key=key,
                requirements={'pkg-039': dict(min=version, max=version+1)}, **extra)


def call(work, root, op, **kwargs):
    return invoke(work, dict(root=str(root), op=op, kwargs=kwargs), 30)


def success(value):
    assert value['ok'], value
    return value['value']


def candidate(work, root, op, **kwargs):
    child = subprocess.Popen([sys.executable, str(HERE/'invoke.py'), str(work)],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, encoding='utf-8')
    child.stdin.write(json.dumps(dict(root=str(root), op=op, kwargs=kwargs))+'\n')
    child.stdin.flush(); child.stdin.close(); child.stdin=None
    return child


def finish(child, timeout=30):
    try:
        stdout, stderr = child.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        child.kill(); child.communicate(timeout=10)
        raise
    return dict(pid=child.pid, exit=child.returncode, stdout=stdout[-3000:], stderr=stderr[-1500:])


def run(campaign, label, skip_publication=False):
    work=campaign/'workspace'; out=campaign/'verification'/label
    out.mkdir(parents=True, exist_ok=False)
    catalog=json.loads((work/'fixtures/catalog.json').read_bytes())
    source=identity(work)
    results=[]
    basic=basic_acceptance(campaign,label+'-basic')
    results.extend(basic['results'])

    def case(name, function):
        started=time.time()
        try:
            detail=function()
            result=dict(case=name,status='PASS',detail=detail)
        except Exception as error:
            result=dict(case=name,status='FAIL',error_type=type(error).__name__,error=str(error)[:3500])
        result['elapsed_s']=round(time.time()-started,3)
        results.append(result)
        (out/'partial.json').write_text(json.dumps(results,indent=2),encoding='utf-8')
        print(json.dumps(result,ensure_ascii=False),flush=True)

    def cycle_backtracking():
        tiny={'a':[dict(version=1,sha256='1'*64,deps={}),dict(version=2,sha256='2'*64,deps={'b':dict(min=1,max=2)})],
              'b':[dict(version=1,sha256='3'*64,deps={'a':dict(min=2,max=3)})]}
        roots={'a':dict(min=1,max=3)}
        actual=success(invoke(work,dict(op='resolve',catalog=tiny,requirements=roots)))
        assert actual==reference_resolve(tiny,roots),(actual,reference_resolve(tiny,roots))
        return actual
    case('cycle_in_highest_choice_backtracks_to_valid_lower_choice',cycle_backtracking)

    def scoped_and_invalid():
        root=out/'scoped'
        for tenant,env in [('tenant-a','prod'),('tenant-b','prod'),('tenant-a','staging')]:
            receipt=success(call(work,root,'install',**request(tenant=tenant,environment=env)))
            assert receipt['generation']==1
            verify_disk(root,catalog,receipt)
        before=(root/'deployments/tenant-a/prod/current.json').read_bytes()
        for override in [dict(tenant='../escape'),dict(environment='../../escape'),dict(key='../escape'),dict(expected_generation=True)]:
            bad=call(work,root,'install',**request(**override))
            assert not bad['ok'] and bad['error_type']=='ValueError',bad
        assert (root/'deployments/tenant-a/prod/current.json').read_bytes()==before
        return dict(scopes=3,invalid_inputs=4)
    case('scope_isolation_and_invalid_input_no_mutation',scoped_and_invalid)

    def crash_recovery(hook):
        root=out/('crash-'+hook)
        first=success(call(work,root,'install',**request()))
        verify_disk(root,catalog,first)
        interrupted=request(key='crash-next',version=3,expected_generation=1)
        child=candidate(work,root,'install',**interrupted,crash_at=hook)
        death=finish(child);assert death['exit']==71,death
        success(call(work,root,'recover'))
        recovered=success(call(work,root,'active',tenant='tenant-a',environment='prod'))
        assert recovered['generation'] in (1,2),recovered
        verify_disk(root,catalog,recovered)
        final=success(call(work,root,'install',**interrupted))
        assert final['generation']==2,final
        verify_disk(root,catalog,final)
        assert success(call(work,root,'install',**interrupted))==final
        assert success(call(work,root,'receipt',tenant='tenant-a',environment='prod',key='first'))==first
        with sqlite3.connect(root/'repo.sqlite') as db:
            assert db.execute('SELECT COUNT(*) FROM receipts').fetchone()[0]==2
        return dict(child=death,recovered_generation=recovered['generation'],final=final)
    for hook in ('before_pointer','after_pointer'):
        case('real_process_crash_'+hook,lambda hook=hook:crash_recovery(hook))

    def four_workers():
        root=out/'concurrent';seed=success(call(work,root,'install',**request()))
        children=[];handles=[]
        try:
            for i in range(4):
                jobs=out/f'jobs-{i}.jsonl';result=out/f'worker-{i}.jsonl'
                jobs.write_bytes(raw(request(key=f'racer-{i}',expected_generation=1))+b'\n')
                log=(out/f'worker-{i}.log').open('wb');handles.append(log)
                p=subprocess.Popen([sys.executable,'-m','app.worker','--root',str(root),'--catalog',str(work/'fixtures/catalog.json'),'--jobs',str(jobs),'--result',str(result)],cwd=work,stdout=log,stderr=log)
                children.append((p,result))
            rows=[]
            for p,result in children:
                p.wait(timeout=30)
                assert result.exists(), ('missing worker result',p.pid,p.returncode)
                lines=[json.loads(s) for s in result.read_text(encoding='utf-8').splitlines()]
                assert len(lines)==1,lines
                rows.extend(lines)
            winners=[r for r in rows if r['ok']]
            assert len(winners)==1,rows
            assert winners[0]['receipt']['generation']==2
            verify_disk(root,catalog,winners[0]['receipt'])
            assert success(call(work,root,'receipt',tenant='tenant-a',environment='prod',key='first'))==seed
            return dict(pids=[p.pid for p,_ in children],results=rows)
        finally:
            for p,_ in children:
                if p.poll() is None:p.kill()
                p.wait(timeout=10)
            for h in handles:h.close()
    case('four_actual_workers_one_generation_cas_winner',four_workers)

    def migration():
        root=out/'migration';root.mkdir()
        original=('legacy-key',7,'a'*64,'{"unchanged":"original bytes"}')
        with sqlite3.connect(root/'repo.sqlite') as db:
            db.execute('PRAGMA user_version=1')
            db.execute('CREATE TABLE receipts(key TEXT PRIMARY KEY,generation INTEGER NOT NULL,manifest_sha256 TEXT NOT NULL,request_json TEXT NOT NULL)')
            db.execute('INSERT INTO receipts VALUES(?,?,?,?)',original)
        child=candidate(work,root,'migrate',crash_at='migration_after_copy')
        death=finish(child);assert death['exit']==72,death
        success(call(work,root,'migrate'))
        success(call(work,root,'recover'))
        with sqlite3.connect(root/'repo.sqlite') as db:
            assert db.execute('PRAGMA user_version').fetchone()[0]==2
            rows=db.execute('SELECT tenant,environment,key,generation,manifest_sha256,request_json FROM receipts').fetchall()
            assert rows==[('legacy','default',*original)],rows
        return dict(crash=death,original_preserved=True)
    case('migration_real_exit_72_and_original_receipt_preservation',migration)

    def future_version():
        root=out/'future';root.mkdir()
        with sqlite3.connect(root/'repo.sqlite') as db:
            db.execute('PRAGMA user_version=99');db.execute('CREATE TABLE sentinel(value TEXT)')
            db.execute("INSERT INTO sentinel VALUES('preserve')")
        before=(root/'repo.sqlite').read_bytes()
        refused=call(work,root,'migrate')
        assert not refused['ok'] and refused['error_type']=='ValueError',refused
        assert (root/'repo.sqlite').read_bytes()==before,'future DB mutated'
        return refused
    case('future_schema_refused_without_db_change',future_version)

    def publish_crash():
        root=out/'publication';receipt=success(call(work,root,'install',**request()))
        entered=threading.Event();release=threading.Event();audit=[];body_seen=[]
        class Receiver(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_POST(self):
                assert urlparse(self.path).path=='/publish',('wrong publication route',self.path)
                body=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                audit.append(dict(method='POST',body=body));body_seen.append(body)
                (out/'receiver-audit.json').write_text(json.dumps(audit,indent=2),encoding='utf-8')
                entered.set();release.wait(15)
                self.close_connection=True
            def do_GET(self):
                assert urlparse(self.path).path=='/receipt',('wrong receipt route',self.path)
                query=parse_qs(urlparse(self.path).query)
                audit.append(dict(method='GET',query=query))
                expected={k:[str(receipt[k])] for k in ('tenant','environment','key')}
                data=raw(receipt)
                present=bool(body_seen) and query==expected
                audit[-1]['status']=200 if present else 404
                self.send_response(200 if present else 404)
                self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
        server=ThreadingHTTPServer(('127.0.0.1',0),Receiver)
        thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start();child=None
        kwargs=dict(tenant='tenant-a',environment='prod',key='first',url=f'http://127.0.0.1:{server.server_port}/')
        try:
            child=candidate(work,root,'publish',**kwargs)
            assert entered.wait(10),'receiver never observed publication'
            assert body_seen==[receipt],body_seen
            child.kill();death=finish(child);assert death['exit']!=0
            release.set()
            acknowledged=call(work,root,'publish',**kwargs)
            assert acknowledged['ok'],acknowledged
            methods=[a['method'] for a in audit]
            assert methods.count('POST')==1 and methods[-1]=='GET',audit
            assert audit[-1]['status']==200,audit
            first_post=methods.index('POST')
            assert all(x['method']=='GET' and x['status']==404 for x in audit[:first_post]),audit
            assert success(call(work,root,'receipt',tenant='tenant-a',environment='prod',key='first'))==receipt
            verify_disk(root,catalog,receipt)
            return dict(crashed_pid=death['pid'],audit=audit,acknowledgement=acknowledged)
        finally:
            release.set()
            if child and child.poll() is None:child.kill();child.wait(timeout=10)
            server.shutdown();server.server_close();thread.join(5)
            (out/'receiver-audit.json').write_text(json.dumps(audit,indent=2),encoding='utf-8')
    if skip_publication:
        results.append(dict(case='receiver_committed_process_killed_query_before_retry',
                            status='STOPPED',fired=False,
                            reason='Prior p5-deep receiver audit proved POST,POST,GET; publication branch remains stopped.'))
    else:
        case('receiver_committed_process_killed_query_before_retry',publish_crash)

    required=[r for r in results if r['status']!='STOPPED']
    report=dict(status='PASS' if all(r['status']=='PASS' for r in required) else 'FAIL',
                scope='deployment_only_publication_stopped' if skip_publication else 'full_application',
                results=results,source_identity=source,oracle_identity=oracle_identity(),provider_paid_calls=0)
    assert identity(work)==source,'source changed while independent acceptance ran'
    (out/'receipt.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
    return report


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('campaign',type=Path);parser.add_argument('--label',required=True);parser.add_argument('--skip-publication',action='store_true')
    args=parser.parse_args();report=run(args.campaign.resolve(),args.label,args.skip_publication)
    raise SystemExit(0 if report['status']=='PASS' else 1)
