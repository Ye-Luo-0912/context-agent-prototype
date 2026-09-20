"""Exclusive v4 campaign preparation; never reseed an existing experiment."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]


def prepare(campaign):
    campaign = Path(campaign).resolve()
    campaign.mkdir(parents=True, exist_ok=False)
    started = time.time()
    caps = dict(schema=1, kind='v4_real_snapshot_workflow', created_epoch=started,
                deadline_epoch=started + 4 * 3600, main_decisions=96,
                provider_attempts=128, tool_attempts=384, input_tokens=4000000,
                output_tokens=250000, estimated_cost_usd=1.0,
                api_protocol='chat', chat_thinking='disabled', model='deepseek-flash')
    (campaign / 'campaign.json').write_text(json.dumps(caps, indent=2) + '\n', encoding='utf-8')
    stage = campaign / 'model'
    subprocess.run([sys.executable, '-B', str(ROOT / 'scripts/package_endurance_v3/prepare.py'),
                    str(stage), '--source', str(ROOT / 'scripts/package_endurance_v3/fixture/app'),
                    '--provenance', 'Accepted ASSISTED v3 base; v4 snapshot module is not implemented'], check=True)
    work = stage / 'workspace'
    shutil.copyfile(Path(__file__).with_name('SPEC.md'), work / 'SPEC.md')
    subprocess.run([sys.executable, '-B', str(Path(__file__).with_name('verify.py')),
                    '--work', str(work), '--seed'], check=True)
    baseline = stage / 'baseline-lock.json'
    shutil.copyfile(baseline, stage / 'v3-preparation-origin.json')
    recorded = json.loads(baseline.read_bytes())
    recorded['files'] = {p.relative_to(work).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
                         for p in work.rglob('*') if p.is_file() and '__pycache__' not in p.parts}
    recorded['v4_provenance'] = 'V4 protected SPEC and fixtures, locked before any model attempt'
    baseline.write_text(json.dumps(recorded, indent=2) + '\n', encoding='utf-8')
    return dict(campaign=str(campaign), stage=str(stage), protected_files=len(recorded['files']),
                candidate_present=(work / 'app/snapshot.py').exists(), **caps)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('campaign', type=Path)
    args = parser.parse_args()
    print(json.dumps(prepare(args.campaign), indent=2))
