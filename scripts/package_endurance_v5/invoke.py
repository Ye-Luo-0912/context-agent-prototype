"""Small controller-side candidate adapter; its output is never an oracle."""
import importlib
import json
import sys
from pathlib import Path

work = Path(sys.argv[1]).resolve()
sys.path.insert(0, str(work))
request = json.loads(sys.stdin.read())

try:
    if request["op"] in {"backup_live", "restore_live"}:
        module = importlib.import_module("app.live_backup")
        value = getattr(module, request["op"])(**request.get("kwargs", {}))
    else:
        from app.repository import Repository
        with Repository(request["root"], work / "fixtures/catalog.json") as repository:
            value = getattr(repository, request["op"])(**request.get("kwargs", {}))
    print(json.dumps({"ok": True, "value": value}, ensure_ascii=False))
except Exception as error:
    print(json.dumps({"ok": False, "error_type": type(error).__name__,
                      "error": str(error)[:4000],
                      "expected_error": isinstance(error, (ValueError, OSError))},
                     ensure_ascii=False))
