"""Explicit, zero-provider, bounded load on an independently accepted candidate."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import threading
import time
import traceback

ROOT = Path(__file__).resolve().parents[2]
WINDOWS = os.name == 'nt'
sys.path.insert(0, str(ROOT/'scripts'))
from runner_process_tree import WindowsLaunchCleanupError, spawn_windows_owned, stop_windows_owned

spec = importlib.util.spec_from_file_location('_snapshot_load_oracle', Path(__file__).with_name('verify.py'))
oracle = importlib.util.module_from_spec(spec)
spec.loader.exec_module(oracle)


def identity(work):
    return {p.relative_to(work).as_posix(): oracle.sha(p.read_bytes())
            for folder in ('app', 'fixtures', 'tests') for p in (work/folder).rglob('*')
            if p.is_file() and '__pycache__' not in p.parts} | {'SPEC.md': oracle.sha((work/'SPEC.md').read_bytes())}


def preflight(stage, acceptance, seconds, *, local_only=False):
    if type(seconds) is not int or not 1 <= seconds <= 300:
        raise ValueError('load duration must be between 1 and 300 seconds')
    if local_only:
        # A separate zero-provider validation of an accepted copy. This mode
        # neither reads nor extends a real-provider campaign or its ledger.
        deadline = time.time()+seconds+180
    else:
        caps = json.loads((stage.parent/'campaign.json').read_bytes())
        if caps.get('kind') != 'v4_real_snapshot_workflow' or caps['deadline_epoch'] != caps['created_epoch']+4*3600:
            raise ValueError('original v4 campaign deadline required')
        deadline = caps['deadline_epoch']
        if deadline-time.time() < seconds+180:
            raise ValueError('campaign deadline does not cover load, crash probes and cleanup')
    work = stage/'workspace'
    receipt = json.loads(acceptance.read_bytes())
    if receipt.get('status') != 'PASS' or not receipt.get('results') or any(row.get('status') != 'PASS' for row in receipt['results']):
        raise ValueError('independent candidate acceptance has not passed')
    if receipt.get('candidate_sha256') != oracle.sha((work/'app/snapshot.py').read_bytes()):
        raise ValueError('candidate changed after independent acceptance')
    if receipt.get('oracle_sha256') != oracle.sha(Path(oracle.__file__).read_bytes()):
        raise ValueError('oracle changed after independent acceptance')
    if receipt.get('source_identity') != identity(work):
        raise ValueError('workspace inputs changed after independent acceptance')
    return work, deadline, identity(work)


def stop(process, cleanups):
    try:
        result = stop_windows_owned(process, 2, 8)
    except Exception as error:
        result = dict(tree_confirmed=False, outcome='unconfirmed', error=repr(error))
    cleanups.append(result)
    return result


def owned_call(command, cleanups, **kwargs):
    process = None;readers=[];output={};overflow=threading.Event()
    try:
        process = spawn_windows_owned(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, **kwargs)
        def read(name, stream):
            output[name]=stream.read(16385)
            if len(output[name])>16384:overflow.set()
        for name in ('stdout','stderr'):
            reader=threading.Thread(target=read,args=(name,getattr(process,name)),daemon=True)
            readers.append(reader);reader.start()
        end=time.monotonic()+30
        while process.poll() is None:
            if overflow.wait(.02):raise ValueError('candidate output exceeds bounded capture')
            if time.monotonic()>=end:raise TimeoutError('candidate operation exceeded 30 seconds')
        for reader in readers:reader.join(1)
        assert not overflow.is_set() and not any(reader.is_alive() for reader in readers)
        return process.returncode, output.get('stdout',''), output.get('stderr','')
    except WindowsLaunchCleanupError as error:
        process = error.process;cleanups.append(error.cleanup)
        raise
    finally:
        if process is not None:
            result = stop(process, cleanups)
            for reader in readers:reader.join(1)
            for stream in (process.stdout, process.stderr):
                if stream is not None:stream.close()
            if not result['tree_confirmed']:raise RuntimeError('owned process cleanup unconfirmed')


def crash_probe(work, folder, archive, members, cleanups):
    folder.mkdir();destination = folder/'restored';before_archive = archive.read_bytes()
    code = 'import sys,json;sys.path.insert(0,sys.argv[1]);from app.snapshot import restore_snapshot;print(json.dumps(restore_snapshot(sys.argv[2],sys.argv[3],crash_at=sys.argv[4] or None)))'
    base = [sys.executable, '-B', '-c', code, str(work), str(archive), str(destination)]
    exit_code, stdout, stderr = owned_call(base+['before_publish'], cleanups, cwd=work)
    assert exit_code == 73, (exit_code, stdout[-2000:], stderr[-2000:])
    assert not destination.exists()
    stages = list(folder.iterdir())
    assert len(stages) == 1 and stages[0].is_dir(), 'crash did not leave a complete unique sibling stage'
    oracle.check_restored(stages[0], members)
    before_retry = oracle.bytes_map(folder)
    exit_code, stdout, stderr = owned_call(base+[''], cleanups, cwd=work)
    assert exit_code == 0 and json.loads(stdout) == oracle.summary(members), (exit_code, stdout[-2000:], stderr[-2000:])
    oracle.check_restored(destination, members)
    oracle.check_only_created(folder, before_retry, [destination.name])
    assert archive.read_bytes() == before_archive
    return dict(real_exit=73, complete_stage=True, retry_verified=True, abandoned_stage_unchanged=True)


class Worker:
    def __init__(self, work, source, folder, cleanups):
        self.folder = folder;folder.mkdir();self.queue = queue.Queue(maxsize=8)
        self.process = None;self.reader = None;self.err_reader=None;self.cleanups = cleanups
        self.log = (folder/'stderr.log').open('wb')
        try:
            self.process = spawn_windows_owned([sys.executable, '-B', str(Path(__file__).resolve()),
                '--worker', str(work), '--source', str(source), '--out', str(folder)],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding='utf-8')
            self.reader = threading.Thread(target=self.read, daemon=True);self.reader.start()
            self.err_reader=threading.Thread(target=self.read_error,daemon=True);self.err_reader.start()
            assert self.receive(15) == {'ready': True}, 'worker startup failed'
        except BaseException as error:
            if isinstance(error, WindowsLaunchCleanupError):self.process=error.process;cleanups.append(error.cleanup)
            self.close();raise

    def read(self):
        try:
            while True:
                line=self.process.stdout.readline(16385)
                if not line:break
                if len(line) > 16384:raise ValueError('worker protocol line exceeds bound')
                self.queue.put(json.loads(line), timeout=1)
        except Exception as error:
            try:self.queue.put(dict(protocol_error=str(error)), timeout=1)
            except queue.Full:pass

    def read_error(self):
        text=self.process.stderr.read(32769)
        self.log.write(text[:32768].encode('utf-8'))
        if len(text)>32768:
            try:self.queue.put(dict(protocol_error='worker stderr exceeds bound'),timeout=1)
            except queue.Full:pass

    def receive(self, timeout):
        return self.queue.get(timeout=max(.001, timeout))

    def send(self, index):
        self.process.stdin.write(json.dumps(dict(index=index))+'\n');self.process.stdin.flush()

    def close(self):
        if self.process is not None:
            try:
                if self.process.stdin is not None:self.process.stdin.close()
            except OSError:pass
            stop(self.process, self.cleanups)
            for reader in (self.reader,self.err_reader):
                if reader is not None:reader.join(2)
                if reader is not None and reader.is_alive():
                    self.cleanups.append(dict(tree_confirmed=False,outcome='unconfirmed',error='worker reader did not join'))
            for stream in (self.process.stdout,getattr(self.process,'stderr',None)):
                if stream is not None:stream.close()
        self.log.close()


def worker(work, source, out):
    sys.path.insert(0, str(work))
    from app.snapshot import export_snapshot, inspect_snapshot, restore_snapshot
    print(json.dumps(dict(ready=True)), flush=True)
    for line in sys.stdin:
        request=json.loads(line);index=request['index'];folder=out/('cycle-%06d'%index);folder.mkdir()
        try:
            a,b,dest=folder/'one.zip',folder/'two.zip',folder/'restored'
            values=[export_snapshot(source,a),export_snapshot(source,b),inspect_snapshot(a),restore_snapshot(a,dest)]
            before=oracle.bytes_map(dest);values.append(restore_snapshot(a,dest))
            result=dict(index=index,values=values,idempotent_unchanged=oracle.bytes_map(dest)==before)
        except Exception as error:
            result=dict(index=index,error=repr(error)[:2000],
                        traceback=traceback.format_exc()[-4000:])
        print(json.dumps(result), flush=True)


def run(stage, out, acceptance, seconds=300, source_label='model_candidate_application_load', *, local_only=False):
    if not WINDOWS:raise OSError('this bounded application load requires Windows Job ownership')
    stage,out,acceptance=map(lambda p:Path(p).resolve(),(stage,out,acceptance))
    out.mkdir(parents=True, exist_ok=False)
    cleanups=[];workers=[];crashes=[];batches=0;verified_cycles=0;load_elapsed=0;fixed=None;started=time.time();status='FAIL';error=None
    accepted_sha=oracle.sha(acceptance.read_bytes()) if acceptance.is_file() else None
    controllers={str(p.relative_to(ROOT)):oracle.sha(p.read_bytes()) for p in (
        Path(__file__).resolve(),Path(__file__).with_name('verify.py').resolve(),ROOT/'scripts/runner_process_tree.py')}
    def receipt():
        value=dict(status=status,source=source_label,provider_paid_calls=0,target_seconds=seconds,
            validation_scope='standalone_local_only' if local_only else 'original_campaign',
            elapsed_seconds=time.time()-started,load_elapsed_seconds=load_elapsed,batches=batches,
            verified_worker_cycles=verified_cycles,crash_probes=crashes,cleanup=cleanups,error=error,
            source_identity=fixed,controller_identity=controllers,
            acceptance_sha256=accepted_sha)
        oracle.write_json(out/'receipt.json',value)
        return value
    try:
        work,deadline,fixed=preflight(stage,acceptance,seconds,local_only=local_only)
        source=work/'fixtures/snapshot-source#one'
        members=oracle.fixture(out/'reference-source');oracle.check_restored(source,members)
        archive=out/'reference.zip';archive.write_bytes(oracle.zip_bytes(members))
        for index in range(2):crashes.append(crash_probe(work,out/('crash-%d'%index),archive,members,cleanups))
        assert identity(work)==fixed, 'candidate or base source changed during crash probes'
        for index in range(4):workers.append(Worker(work,source,out/('worker-%d'%index),cleanups))
        load_started=time.time();end=load_started+seconds
        assert end+45 < deadline, 'remaining campaign deadline cannot cover load and cleanup'
        status='RUNNING';receipt()
        while time.time()<end:
            cycle=time.time()
            if batches and end-cycle<1:
                time.sleep(max(0,end-time.time()));break
            for item in workers:item.send(batches)
            for item in workers:
                row=item.receive(min(30,end-time.time(),deadline-time.time()-45));assert row.get('index')==batches and not row.get('error'),row
                assert row.get('values')==[oracle.summary(members)]*5 and row.get('idempotent_unchanged') is True,row
                folder=item.folder/('cycle-%06d'%batches)
                assert {p.name for p in folder.iterdir()}=={'one.zip','two.zip','restored'}
                for name in ('one.zip','two.zip'):oracle.check_archive(folder/name,members)
                assert (folder/'one.zip').read_bytes()==(folder/'two.zip').read_bytes()
                oracle.check_restored(folder/'restored',members)
                verified_cycles+=1
                assert folder.resolve().parent==item.folder.resolve(), 'cycle cleanup escaped owned directory'
                shutil.rmtree(folder)
            assert identity(work)==fixed, 'candidate or base source changed during load'
            batches+=1;load_elapsed=time.time()-load_started;receipt()
            with (out/'timeline.jsonl').open('a',encoding='utf-8') as log:
                log.write(json.dumps(dict(batch=batches,elapsed=load_elapsed,verified_workers=4))+'\n')
            time.sleep(max(0,min(1-(time.time()-cycle),end-time.time())))
        load_elapsed=time.time()-load_started
        assert batches>0, 'no complete four-worker load cycle was verified'
        assert identity(work)==fixed
        assert oracle.sha(acceptance.read_bytes())==accepted_sha, 'acceptance receipt changed during load'
        assert all(oracle.sha((ROOT/name).read_bytes())==digest for name,digest in controllers.items()), 'controller changed during load'
        assert all(row['tree_confirmed'] for row in cleanups), 'crash cleanup unconfirmed'
        status='PASS'
    except BaseException as exc:error=dict(type=type(exc).__name__,message=str(exc));status='FAIL'
    finally:
        for item in workers:
            try:item.close()
            except Exception as exc:cleanups.append(dict(tree_confirmed=False,outcome='unconfirmed',error=str(exc)))
        if any(not row.get('tree_confirmed') for row in cleanups):status='CLEANUP_UNCONFIRMED'
    return receipt()


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--worker',type=Path);parser.add_argument('--source',type=Path)
    parser.add_argument('--stage',type=Path);parser.add_argument('--out',type=Path,required=True)
    parser.add_argument('--acceptance',type=Path);parser.add_argument('--seconds',type=int,default=300)
    parser.add_argument('--source-label',choices=['model_candidate_application_load','assisted_application_load'],default='model_candidate_application_load')
    parser.add_argument('--local-only',action='store_true',help='Separate zero-provider validation; never resume or extend a campaign')
    args=parser.parse_args()
    if args.worker:worker(args.worker,args.source,args.out)
    else:
        if not args.stage or not args.acceptance:parser.error('--stage and --acceptance are required')
        result=run(args.stage,args.out,args.acceptance,args.seconds,args.source_label,local_only=args.local_only)
        print(json.dumps({k:v for k,v in result.items() if k!='source_identity'}))
        raise SystemExit(0 if result['status']=='PASS' else 1)
