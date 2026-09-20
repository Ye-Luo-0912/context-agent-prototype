"""One long-lived application connection; controlled stdin commands, real effects."""
import argparse
import json
import os
import sys
import time
from pathlib import Path


def emit(value):print(json.dumps(value,ensure_ascii=False),flush=True)


def main():
    p=argparse.ArgumentParser();p.add_argument('--work',type=Path,required=True);p.add_argument('--root',type=Path,required=True)
    a=p.parse_args();sys.path.insert(0,str(a.work.resolve()))
    from app.repository import Repository
    from app import deploy
    with Repository(a.root,a.work/'fixtures/catalog.json') as repo:
        emit(dict(phase='ready',pid=os.getpid()))
        for line in sys.stdin:
            message=json.loads(line)
            if message['op']=='stop':break
            started=time.time();emit(dict(phase='entered',id=message['id'],op=message['op'],time=started))
            original=deploy.atomic_write_json
            gate=message.get('gate')
            if gate:
                def gated(path,value):
                    if Path(path).name!='current.json' or value.get('key')!=message['kwargs']['key']:
                        return original(path,value)
                    if gate=='after':original(path,value)
                    emit(dict(phase='pointer_gate',id=message['id'],mode=gate,time=time.time(),pid=os.getpid()))
                    # The controller kills the already-observed process at
                    # this real disk boundary. A timeout is a failure, not a pass.
                    time.sleep(60)
                    raise TimeoutError('pointer gate was not cancelled by controller')
                deploy.atomic_write_json=gated
            try:
                if message['op']=='install':value=repo.install(**message['kwargs'])
                elif message['op']=='gc':value=repo.gc()
                elif message['op']=='recover':value=repo.recover()
                else:raise ValueError('unknown controlled operation')
                emit(dict(phase='result',id=message['id'],ok=True,value=value,started=started,finished=time.time()))
            except Exception as error:
                emit(dict(phase='result',id=message['id'],ok=False,error_type=type(error).__name__,error=str(error),started=started,finished=time.time()))
            finally:deploy.atomic_write_json=original


if __name__=='__main__':main()
