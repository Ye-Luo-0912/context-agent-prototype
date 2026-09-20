"""Observe actual pointer and durable receipts before/after old-key replay."""
import argparse
import json
import sqlite3
from pathlib import Path

from exercise import call, request, success
from verify import verify_disk


def run(campaign):
    work=campaign/'workspace';out=campaign/'replay-regression'
    out.mkdir(exist_ok=False);root=out/'repository'
    catalog=json.loads((work/'fixtures/catalog.json').read_bytes());steps=[]
    for name,kwargs in [('install_first',request()),
                        ('install_second',request(key='second',version=3,expected_generation=1)),
                        ('replay_first',request())]:
        receipt=success(call(work,root,'install',**kwargs))
        pointer=json.loads((root/'deployments/tenant-a/prod/current.json').read_bytes())
        with sqlite3.connect(root/'repo.sqlite') as db:
            receipts=db.execute('SELECT key,generation,manifest_sha256 FROM receipts ORDER BY generation').fetchall()
        verify_disk(root,catalog,receipt,active=False)
        steps.append(dict(action=name,request=kwargs,returned=receipt,actual_pointer=pointer,durable_receipts=receipts))
    assert [s['returned']['generation'] for s in steps]==[1,2,1],steps
    assert steps[1]['actual_pointer']['generation']==2,steps
    reproduced=steps[-1]['actual_pointer']['generation']==1
    report=dict(status='REPRODUCED' if reproduced else 'NOT_REPRODUCED',
                defect='old_idempotency_replay_reactivates_old_deployment',steps=steps,
                provider_paid_calls=0)
    (out/'receipt.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
    print(json.dumps(report,ensure_ascii=False))


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('campaign',type=Path)
    run(parser.parse_args().campaign.resolve())
