"""Counterexample: public WorkSteer char budget versus host decoded byte budget."""
import argparse
import hashlib
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from host_journey import Host, final


def run(out):
    out.mkdir(parents=True,exist_ok=False)
    class Provider(BaseHTTPRequestHandler):
        def log_message(self,*args):pass
        def do_POST(self):
            self.rfile.read(int(self.headers['Content-Length']))
            data=final('host length probe observed')
            self.send_response(200);self.send_header('Content-Type','text/event-stream')
            self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
    server=ThreadingHTTPServer(('127.0.0.1',0),Provider)
    thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    cases=[]
    try:
        for family,values in [('ascii',['x'*4096,'x'*4097]),('utf8',['汉'*1365,'汉'*1366])]:
            work=out/family/'workspace';work.mkdir(parents=True)
            host=Host(work,out/family/'process',server.server_port)
            try:
                task=host.rpc('submit',dict(goal='host transport limit probe',client_request_id=family))['task_id'];host.idle()
                for text in values:
                    row=dict(family=family,chars=len(text),utf8_bytes=len(text.encode()),
                             sha256=hashlib.sha256(text.encode()).hexdigest(),
                             within_public_200000_char_contract=True)
                    try:
                        result=host.rpc('steer',dict(instruction=text,expected_task_id=task))
                        host.idle();row.update(outcome='accepted',receipt=result)
                    except EOFError as error:
                        row.update(outcome='connection_closed',error=str(error),host_still_alive=host.process.poll() is None)
                    cases.append(row)
            finally:host.stop()
        expected=['accepted','connection_closed','accepted','connection_closed']
        assert [r['outcome'] for r in cases]==expected,cases
        assert all(r.get('host_still_alive',True) for r in cases),cases
        report=dict(status='REPRODUCED',defect='public_work_text_vs_host_decoder_budget',cases=cases,provider_paid_calls=0)
    finally:
        server.shutdown();server.server_close();thread.join(5)
    (out/'receipt.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
    print(json.dumps(report,ensure_ascii=False))


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('out',type=Path)
    run(parser.parse_args().out.resolve())
