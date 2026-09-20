"""Create a new isolated stage from deterministic data and a selected app source."""
import argparse
import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

REPO=Path(__file__).resolve().parents[2]


def prepare(stage, source, provenance):
    stage=stage.resolve();source=source.resolve()
    subprocess.run([sys.executable,str(REPO/'scripts/package_endurance/prepare.py'),str(stage)],check=True)
    copied={}
    for path in source.rglob('*'):
        if not path.is_file() or '__pycache__' in path.parts:continue
        if not path.resolve().is_relative_to(source):raise ValueError('source path escaped app root')
        rel=path.relative_to(source);dest=stage/'workspace/app'/rel
        dest.parent.mkdir(parents=True,exist_ok=True);shutil.copyfile(path,dest)
        copied[rel.as_posix()]=hashlib.sha256(dest.read_bytes()).hexdigest()
    # Original fixture baseline remains immutable; this explicitly records
    # the chosen app input rather than pretending the copied code was v1.
    identity=dict(source=str(source),provenance=provenance,app_sha256=copied)
    (stage/'app-baseline.json').write_text(json.dumps(identity,indent=2)+'\n',encoding='utf-8')
    baseline=stage/'baseline-lock.json'
    shutil.copyfile(baseline,stage/'fixture-origin.json')
    recorded=json.loads(baseline.read_bytes())
    recorded['files']={p.relative_to(stage/'workspace').as_posix():hashlib.sha256(p.read_bytes()).hexdigest()
                       for p in (stage/'workspace').rglob('*') if p.is_file() and '__pycache__' not in p.parts}
    recorded['app_provenance']=provenance
    baseline.write_text(json.dumps(recorded,indent=2)+'\n',encoding='utf-8')
    return identity


if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('stage',type=Path)
    parser.add_argument('--source',type=Path,required=True);parser.add_argument('--provenance',required=True)
    args=parser.parse_args();print(json.dumps(prepare(args.stage,args.source,args.provenance)))
