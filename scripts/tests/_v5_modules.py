"""Load the frozen V5 modules by file path.

The shared ``scripts/tests`` suite puts several sibling directories on
``sys.path``, and those directories also contain modules named ``oracle`` and
``run``. Loading the V5 modules from their own files — with the V5 directory
first on the path only while they import their siblings — keeps this regression
pointed at the files it claims to test, and leaves whatever the other suites
cached exactly as it was.
"""
from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

TESTS = Path(__file__).resolve().parent
SCRIPTS = TESTS.parent
REPO = SCRIPTS.parent
V5 = SCRIPTS / "package_endurance_v5"

ALIASES = ("oracle", "workload", "runner_grants", "campaign_accounting", "invoke")

_cache: dict = {}


def import_v5(name: str):
    """Import ``scripts/package_endurance_v5/<name>.py`` under a private name."""
    if name in _cache:
        return _cache[name]
    saved = {key: sys.modules.pop(key) for key in ALIASES if key in sys.modules}
    if str(V5) in sys.path:
        sys.path.remove(str(V5))
    sys.path.insert(0, str(V5))
    try:
        spec = importlib.util.spec_from_file_location(f"v5_{name}", V5 / f"{name}.py")
        if spec is None or spec.loader is None:
            raise RuntimeError(f"cannot load {name} from {V5}")
        module = importlib.util.module_from_spec(spec)
        sys.modules[f"v5_{name}"] = module
        spec.loader.exec_module(module)
    finally:
        sys.path.remove(str(V5))
        for key in ALIASES:
            sys.modules.pop(key, None)
        sys.modules.update(saved)
    _cache[name] = module
    return module
