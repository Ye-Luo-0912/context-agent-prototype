"""Exclusive fixture generation. Never reseeds an existing campaign."""
import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False, allow_nan=False).encode()


def put(root, relative, data):
    path = root / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data if isinstance(data, bytes) else data.encode())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("campaign", type=Path)
    args = parser.parse_args()
    campaign = args.campaign.resolve()
    campaign.mkdir(parents=True, exist_ok=False)
    work = campaign / "workspace"
    work.mkdir()
    catalog = {}
    for i in range(120):
        name = f"pkg-{i:03}"
        catalog[name] = []
        for version in range(1, 5):
            payload = (f"package={name};version={version};\n" + "数据-package-line\n" * 40).encode()
            digest = hashlib.sha256(payload).hexdigest()
            deps = {} if i == 0 else {f"pkg-{i-1:03}": {"min": version, "max": version + 1}}
            if i > 3 and i % 3 == 0:
                deps[f"pkg-{i-3:03}"] = {"min": version, "max": version + 1}
            catalog[name].append(dict(version=version, sha256=digest, deps=deps))
            put(work, f"fixtures/blobs/{digest}", payload)
    put(work, "fixtures/catalog.json", canonical(catalog))
    put(work, "SPEC.md", (Path(__file__).parent / "spec.md").read_bytes())
    put(work, "app/__init__.py", "")
    put(work, "app/repository.py", '''"""Runnable v1: exact single-package resolution; upgrade per SPEC.md."""
import hashlib
import json
from pathlib import Path

def resolve(catalog, requirements):
    result = {}
    for name, requirement in sorted(requirements.items()):
        choices = [v for v in catalog.get(name, []) if requirement['min'] <= v['version'] < requirement['max']]
        if not choices:
            raise ValueError('no version')
        result[name] = max(v['version'] for v in choices)
    return result

class Repository:
    def __init__(self, root, catalog_path):
        self.root = Path(root)
        self.catalog_path = Path(catalog_path)
        self.catalog = json.loads(self.catalog_path.read_text(encoding='utf-8'))
    def __enter__(self): return self
    def __exit__(self, *args): self.close()
    def close(self): pass
    def install(self, *args, **kwargs): raise NotImplementedError('v2 deployment pending')
    def recover(self): raise NotImplementedError('v2 recovery pending')
    def active(self, *args): return None
    def receipt(self, *args): return None
    def gc(self): return []
    def migrate(self, *args, **kwargs): raise NotImplementedError('v2 migration pending')
    def publish(self, *args, **kwargs): raise NotImplementedError('v2 publication pending')
''')
    put(work, "tests/test_public.py", '''import unittest
from app.repository import resolve

class PublicV1(unittest.TestCase):
    def test_single_package(self):
        catalog = {'a': [{'version': 1, 'sha256': '1'*64, 'deps': {}}, {'version': 2, 'sha256': '2'*64, 'deps': {}}]}
        self.assertEqual(resolve(catalog, {'a': {'min': 1, 'max': 3}}), {'a': 2})
    def test_missing_root_refused(self):
        with self.assertRaises(ValueError): resolve({}, {'a': {'min': 1, 'max': 2}})
''')
    files = {p.relative_to(work).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest() for p in work.rglob('*') if p.is_file()}
    head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=REPO, text=True).strip()
    binary = REPO / 'target/debug/agent-tui.exe'
    put(campaign, 'baseline-lock.json', canonical(dict(head=head, runtime_binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), files=files)))
    put(campaign, 'fixture-index.json', canonical(dict(packages=120, versions=480, files=len(files), immutable=[n for n in files if not n.startswith('app/')])) )
    print(json.dumps({'campaign': str(campaign), 'packages': 120, 'versions': 480, 'files': len(files)}))

if __name__ == '__main__': main()
