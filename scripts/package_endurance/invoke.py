"""Candidate adapter, deliberately separate from independent disk oracle."""
import json
import sys
from pathlib import Path

WORK = Path(sys.argv[1]).resolve()
sys.path.insert(0, str(WORK))


def call(request):
    from app.repository import Repository, resolve
    if request['op'] == 'resolve':
        return resolve(request['catalog'], request['requirements'])
    with Repository(request['root'], WORK / 'fixtures/catalog.json') as repo:
        return getattr(repo, request['op'])(**request.get('kwargs', {}))


for line in sys.stdin:
    try:
        value = call(json.loads(line))
        reply = {'ok': True, 'value': value}
    except Exception as error:
        reply = {'ok': False, 'error_type': type(error).__name__, 'message': str(error)[:2000]}
    print(json.dumps(reply, ensure_ascii=False), flush=True)
