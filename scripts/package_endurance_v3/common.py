"""Content identities and immutable, content-based campaign deadlines."""
import hashlib
import json
import time
from pathlib import Path

REPO=Path(__file__).resolve().parents[2]


def write_json(path, value):
    path=Path(path);path.parent.mkdir(parents=True,exist_ok=True)
    temp=path.with_name(path.name+'.tmp')
    temp.write_text(json.dumps(value,ensure_ascii=False,indent=2)+'\n',encoding='utf-8')
    temp.replace(path)


def identity(work):
    work=Path(work)
    return {p.relative_to(work).as_posix():hashlib.sha256(p.read_bytes()).hexdigest()
            for folder in ('app','fixtures','tests') for p in (work/folder).rglob('*')
            if p.is_file() and '__pycache__' not in p.parts} | {
                'SPEC.md':hashlib.sha256((work/'SPEC.md').read_bytes()).hexdigest()}


def create_campaign(campaign, started_epoch):
    manifest=Path(campaign)/'campaign.json'
    data=dict(schema=1,created_epoch=started_epoch,deadline_epoch=started_epoch+4*3600,
              main_decisions=48,provider_attempts=64,tool_attempts=192,
              estimated_cost_usd=1.0,load_seconds=1800)
    with manifest.open('x',encoding='utf-8') as handle:
        json.dump(data,handle,indent=2);handle.write('\n')
    return data


def campaign_limits(stage):
    data=json.loads((Path(stage).parent/'campaign.json').read_bytes())
    assert data['deadline_epoch']==data['created_epoch']+4*3600,'altered campaign duration'
    return data


def assert_identity(work, expected):
    actual=identity(work)
    if actual!=expected:
        changed=sorted(k for k in actual.keys()|expected.keys() if actual.get(k)!=expected.get(k))
        raise ValueError('source/fixture identity changed: '+str(changed[:20]))


def oracle_identity():
    names=['scripts/package_endurance_v3/acceptance.py','scripts/package_endurance_v3/common.py',
           'scripts/package_endurance/verify.py','scripts/package_endurance/invoke.py']
    return {name:hashlib.sha256((REPO/name).read_bytes()).hexdigest() for name in names}
