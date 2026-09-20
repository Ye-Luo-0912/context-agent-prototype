"""Actual public named-pipe host journey with a tagged synthetic provider.

All task changes go through work APIs. No Runtime state is edited by this
controller. Raw RPCs/requests and process identities are retained as evidence.
"""
import argparse
import hashlib
import json
import os
import queue
import struct
import subprocess
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

REPO=Path(__file__).resolve().parents[2]


class Host:
    def __init__(self, work, out, port, restore=False):
        self.work=work;self.out=out;out.mkdir()
        self.pipe='package-endurance-'+uuid.uuid4().hex
        env={k:v for k,v in os.environ.items() if not k.startswith(('OPENAI_','MAINTENANCE_')) and k not in ('AGENT_DEMO','AGENT_AUTO_APPROVE')}
        env.update(OPENAI_API_KEY='local-synthetic-only',OPENAI_BASE_URL=f'http://127.0.0.1:{port}/v1',OPENAI_MODEL='local-package-fault-provider',OPENAI_API_PROTOCOL='responses',OPENAI_RESPONSES_REASONING_EFFORT='none',OPENAI_MAX_OUTPUT_TOKENS='8192',MAINTENANCE_MAX_CALLS_PER_MAINTAIN='0',NO_PROXY='127.0.0.1,localhost')
        self.log=(out/'host.log').open('wb')
        cmd=[str(REPO/'target/debug/agent-host.exe'),'--workdir',str(work),'--pipe',self.pipe,'--context-policy','dynamic','--max-rounds','8']
        if restore:cmd.append('--restore-latest')
        self.process=subprocess.Popen(cmd,env=env,stdout=self.log,stderr=self.log,creationflags=subprocess.CREATE_NO_WINDOW)
        self.birth=time.time();self.stream=None
        deadline=time.monotonic()+25
        while time.monotonic()<deadline:
            if self.process.poll() is not None:raise RuntimeError('host failed to start: '+(out/'host.log').read_text(errors='replace')[-1800:])
            try:self.stream=open('\\\\.\\pipe\\'+self.pipe,'r+b',buffering=0);break
            except OSError:time.sleep(.1)
        if self.stream is None:
            self.stop();raise TimeoutError('named pipe unavailable')
        self.sequence=0

    def _exchange(self,envelope,result):
        try:
            data=json.dumps(envelope).encode();self.stream.write(struct.pack('<I',len(data))+data)
            def read(n):
                chunks=[]
                while n:
                    piece=self.stream.read(n)
                    if not piece:raise EOFError('host closed mid-frame')
                    chunks.append(piece);n-=len(piece)
                return b''.join(chunks)
            size=struct.unpack('<I',read(4))[0]
            if size>4*1024*1024:raise ValueError('frame over bound')
            result.put((True,json.loads(read(size))))
        except Exception as error:result.put((False,error))

    def rpc(self,operation,payload=None,namespace='work',timeout=10):
        mid=str(uuid.uuid4());rid=str(uuid.uuid4());self.sequence+=1
        envelope=dict(protocol=dict(name='focus-agent.platform',version=dict(major=1,minor=0),active_features=[],schema_digest=hashlib.sha256(b'focus-agent.platform.work.v1|run-scoped').hexdigest()),message_id=mid,request_id=rid,kind='request',route=dict(namespace=namespace,operation=operation),causality=dict(correlation_id=mid),payload=payload or {})
        # Persist the request before I/O: a disconnected request is evidence too.
        with (self.out/'sent.jsonl').open('a',encoding='utf-8') as log:
            log.write(json.dumps(envelope)+'\n')
        result=queue.Queue();thread=threading.Thread(target=self._exchange,args=(envelope,result),daemon=True);thread.start()
        started=time.monotonic()
        try:ok,response=result.get(timeout=timeout)
        except queue.Empty:
            self.stop();thread.join(3);raise TimeoutError(f'{namespace}.{operation} did not respond within {timeout}s')
        elapsed=time.monotonic()-started
        if not ok:raise response
        assert response['request_id']==rid and response['kind']=='response'
        with (self.out/'rpc.jsonl').open('a',encoding='utf-8') as log:log.write(json.dumps(dict(request=envelope,response=response,elapsed=elapsed))+'\n')
        if response['payload']['status']!='success':raise RuntimeError(response['payload'])
        return response['payload']['value']

    def idle(self,timeout=20):
        end=time.monotonic()+timeout
        while time.monotonic()<end:
            state=self.rpc('snapshot')
            # This synthetic lane emits no writes unless the test explicitly
            # drives them. Never auto-approve unexpected requests.
            assert not state.get('pending_approvals'), state.get('pending_approvals')
            reason=(state.get('continue_readiness') or {}).get('reason')
            if reason!='turn_running':return state
            time.sleep(.05)
        raise TimeoutError('host did not reach a typed idle state')

    def stop(self):
        if getattr(self,'process',None) is not None:
            if self.process.poll() is None:self.process.kill()
            self.process.wait(timeout=10)
        if getattr(self,'stream',None):self.stream.close();self.stream=None
        if getattr(self,'log',None):self.log.close()


def sse(items):return ''.join('data: '+json.dumps(item)+'\n\n' for item in items).encode()
def final(text='controlled boundary complete'):
    return sse([dict(type='response.output_text.delta',output_index=0,delta=text),dict(type='response.output_item.done',output_index=0,item=dict(type='message',role='assistant',content=[dict(type='output_text',text=text)])),dict(type='response.completed',response=dict(status='completed',usage=dict(input_tokens=44,output_tokens=8)))])


def journey(work,out,restore,directive_repeats=36):
    out.mkdir(parents=True,exist_ok=False);work.mkdir(parents=True,exist_ok=True)
    state={'mode':'normal','requests':[]};lock=threading.Lock();entered=threading.Event();release=threading.Event()
    class Provider(BaseHTTPRequestHandler):
        def log_message(self,*_):pass
        def do_POST(self):
            request=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
            with lock:
                mode=state['mode'];reply_text=state.get('hold_text','held result returned');number=len(state['requests'])+1;state['requests'].append(dict(mode=mode,request=request))
            (out/f'request-{number:03}.json').write_text(json.dumps(request,ensure_ascii=False),encoding='utf-8')
            if mode=='hold':
                entered.set()
                if not release.wait(25):data=final('hold timed out')
                else:data=final(reply_text)
            elif mode=='malformed':
                data=sse([dict(type='response.output_text.delta',output_index=0,delta='visible boundary marker'),dict(type='response.output_item.done',output_index=1,item=dict(type='function_call',name='fs_write',call_id='invalid-tool',arguments='{"path":')),dict(type='response.completed',response=dict(status='completed',usage=dict(input_tokens=44,output_tokens=8)))])
            else:data=final()
            try:
                self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
            except (BrokenPipeError,ConnectionResetError):pass
    server=ThreadingHTTPServer(('127.0.0.1',0),Provider);thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    host=None;receipts=[];pids=[]
    def record(case,detail):receipts.append(dict(case=case,status='PASS',detail=detail));(out/'partial.json').write_text(json.dumps(receipts,indent=2),encoding='utf-8')
    try:
        host=Host(work,out/'process-1',server.server_port,restore);pids.append(host.process.pid)
        snap=host.rpc('snapshot')
        if restore:
            assert snap['focus'];t1=snap['focus']['task_id']
        else:
            t1=host.rpc('submit',dict(goal='T1 package repository boundary fixture',client_request_id='t1'))['task_id'];host.idle()
        t2=host.rpc('submit',dict(goal='T2 isolated read-only fixture',client_request_id='t2'))['task_id'];host.idle();assert t1!=t2
        host.rpc('activate',dict(task_id=t1));assert host.rpc('snapshot')['focus']['task_id']==t1
        wrong=host.rpc('steer',dict(instruction='must not land on T1',expected_task_id=t2));assert wrong['disposition']=='rejected',wrong
        record('T1_T2_T1_wrong_task_refused',dict(t1=t1,t2=t2,wrong=wrong))
        state['mode']='hold';entered.clear();release.clear()
        host.rpc('steer',dict(instruction='Hold this T1 turn for controller queue checks.',expected_task_id=t1));assert entered.wait(10)
        correction='QUEUED-CORRECTION-v2: preserve canonical package hashes and receipt identity.'
        queued=host.rpc('steer',dict(instruction=correction,expected_task_id=t1));assert queued['disposition']=='queued',queued
        saturated=host.rpc('steer',dict(instruction='extra correction must be refused',expected_task_id=t1));assert saturated['disposition']=='rejected',saturated
        state['mode']='normal';release.set();host.idle()
        assert correction in json.dumps(state['requests'][-1]['request'])
        record('running_steer_queue_saturation',dict(queued=queued,saturated=saturated))
        # The original 120-repeat probe is retained in host-calibration.
        # The host currently rejects decoded strings over 4096 bytes although
        # the public WorkSteerRequest permits 200000 characters. Exercise cold
        # restore within the actual transport bound; do not call that defect fixed.
        directive='BEGIN-PACKAGE-v2 '+('Retain package hash checks, original receipt identities, and tenant isolation. '*directive_repeats)+' END-PACKAGE-v2'
        state['mode']='malformed';before=len(state['requests'])
        host.rpc('steer',dict(instruction=directive,expected_task_id=t1));host.idle()
        assert len(state['requests'])-before==1,'visible response must not be automatically replayed'
        checkpoint=host.rpc('checkpoint');host.stop();host=None
        state['mode']='normal'
        host=Host(work,out/'process-2',server.server_port,True);pids.append(host.process.pid)
        assert host.rpc('snapshot')['focus']['task_id']==t1
        before=len(state['requests']);host.rpc('continue');host.idle()
        assert directive in json.dumps(state['requests'][before]['request'],ensure_ascii=False),'cold restore lost current directive'
        record('visible_malformed_then_cold_restore',dict(checkpoint=checkpoint,directive_bytes=len(directive.encode()),directive_sha256=hashlib.sha256(directive.encode()).hexdigest(),task_id=t1))
        late_marker='LATE-CANCEL-REPLY-'+uuid.uuid4().hex
        state['hold_text']=late_marker;state['mode']='hold';entered.clear();release.clear()
        host.rpc('steer',dict(instruction='Cancel this held operation; do not adopt its late content.',expected_task_id=t1));assert entered.wait(10)
        start=time.monotonic();cancel=host.rpc('cancel',dict(expected_task_id=t1));latency=time.monotonic()-start
        state['mode']='normal';release.set();snapshot=host.idle()
        assert cancel['ack']['status']=='cancelled' and latency<10,cancel
        host.rpc('continue');host.idle()
        assert late_marker not in json.dumps(state['requests'][-1]['request']),'cancelled content adopted by next model input'
        record('cancel_held_model_and_late_reply',dict(cancel=cancel,latency_s=latency,snapshot=snapshot,late_marker=late_marker,next_request_excludes_late_content=True))
        for n in (3,4):
            checkpoint=host.rpc('checkpoint');host.stop();host=None
            host=Host(work,out/f'process-{n}',server.server_port,True);pids.append(host.process.pid)
            assert host.rpc('snapshot')['focus']['task_id']==t1
        record('four_actual_host_processes',dict(pids=pids,task_id=t1))
        trace_rows=[json.loads(line) for path in (work/'.focus-agent/traces').glob('*.jsonl') for line in path.read_text(encoding='utf-8').splitlines()]
        trace_events=[row.get('event',{}) for row in trace_rows]
        assert any(e.get('type')=='assistant_message' and e.get('content')=='controlled boundary complete' for e in trace_events),'synthetic provider never delivered actual text'
        assert any(e.get('type')=='turn_failed' and e.get('task_id')==t1 for e in trace_events),'malformed response did not publish typed failure'
        cancellations=[e for e in trace_events if e.get('type')=='turn_cancelled' and e.get('turn_id')==cancel['ack']['turn_id']]
        assert len(cancellations)==1,cancellations
        assert not any(e.get('type')=='assistant_message' and late_marker in e.get('content','') for e in trace_events),'late reply published as assistant text'
        record('typed_failure_cancel_and_real_text_observed',dict(cancelled_turn_id=cancel['ack']['turn_id'],cancel_terminals=1,late_assistant_text=False))
        report=dict(status='PASS',source='synthetic_provider_real_host_public_protocol',cases=receipts,provider_paid_calls=0,directive_repeats=directive_repeats)
    except Exception as error:
        report=dict(status='FAIL',source='synthetic_provider_real_host_public_protocol',cases=receipts,error_type=type(error).__name__,error=str(error),pids=pids)
    finally:
        release.set()
        if host is not None:host.stop()
        server.shutdown();server.server_close();thread.join(5)
    (out/'receipt.json').write_text(json.dumps(report,indent=2),encoding='utf-8');print(json.dumps(report,ensure_ascii=False));return report['status']=='PASS'


def main():
    ap=argparse.ArgumentParser();ap.add_argument('work',type=Path);ap.add_argument('out',type=Path);ap.add_argument('--restore',action='store_true');ap.add_argument('--directive-repeats',type=int,default=36);a=ap.parse_args()
    return 0 if journey(a.work.resolve(),a.out.resolve(),a.restore,a.directive_repeats) else 1

if __name__=='__main__':raise SystemExit(main())
