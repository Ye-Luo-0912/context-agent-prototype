
import json
from pathlib import Path

def expected():
    rows=[json.loads(line) for line in (Path(__file__).parent/'fixtures/input.jsonl').read_text().splitlines() if line]
    return sorted([r for r in rows if r['kind']=='a'], key=lambda r:r['amount'])

if __name__=='__main__':
    print(json.dumps(expected(), sort_keys=True))
