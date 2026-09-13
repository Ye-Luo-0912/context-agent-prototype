"""Read-only source inventory/reading evidence for this audit; no builds/API calls."""
import hashlib
import json
import re
from pathlib import Path
import subprocess
import sys

sys.stdout.reconfigure(encoding="utf-8")

ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent
MANIFEST = HERE / "SOURCE_MANIFEST.json"
READS = HERE / "READ_RANGES.jsonl"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_files():
    raw = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=ROOT
    ).decode("utf-8").split("\0")
    suffixes = {".rs", ".cs", ".axaml", ".py", ".sh", ".ps1", ".yml", ".toml"}
    for name in sorted(set(raw)):
        p = Path(name)
        if not name or p.suffix not in suffixes:
            continue
        if not (name.startswith(("crates/", "apps/", "clients/", "scripts/", ".github/"))
                or name in {"Cargo.toml", "rust-toolchain.toml"}):
            continue
        if any(x in p.parts for x in ("evidence", "seeds", "seed", "golden", "suite", "obj", "bin")):
            continue
        if (ROOT / p).is_file():
            yield name


if sys.argv[1] == "snapshot":
    if MANIFEST.exists():
        raise SystemExit("Refusing to overwrite audit baseline")
    files = [{"path": n, "sha256": digest(ROOT / n),
              "lines": len((ROOT / n).read_text(encoding="utf-8-sig").splitlines())}
             for n in source_files()]
    data = {"head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT).decode().strip(),
            "scope": "Product/library/test/build sources; fixtures, frozen evidence and generated outputs excluded",
            "files": files}
    MANIFEST.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"files": len(files), "lines": sum(x["lines"] for x in files)}))
elif sys.argv[1] == "scan":
    patterns = {
        "await_boundary": r"\.await\b|\bawait\s",
        "blocking_or_whole_read": r"read_to_string|read_to_end|fs::read\(|\.Result\b|\.Wait\(",
        "bounds_and_cancellation": r"MAX_|CAP|[Cc]ancel|timeout|[Bb]udget|[Ww]atermark",
        "collection_mutation": r"\.push\(|\.insert\(|\.extend\(|\.Add\(",
        "unfinished_marker": r"TODO|FIXME|todo!|unimplemented!",
        "function_or_type": r"\bfn\s+|\b(?:struct|enum|trait|class|interface)\s+",
    }
    rows = []
    modules = {}
    for name in source_files():
        lines = (ROOT / name).read_text(encoding="utf-8-sig").splitlines()
        matches = {key: [i + 1 for i, line in enumerate(lines) if re.search(pattern, line)]
                   for key, pattern in patterns.items()}
        rows.append({"path": name, "matches": matches})
        module = "/".join(name.split("/")[:2]) if "/" in name else name
        stat = modules.setdefault(module, {"files": 0, "lines": 0})
        stat["files"] += 1
        stat["lines"] += len(lines)
    (HERE / "STRUCTURAL_SCAN.json").write_text(json.dumps({
        "meaning": "Lexical scan over every inventoried source; matches include tests/comments and are not defects or semantic review coverage",
        "patterns": patterns, "modules": modules, "files": rows,
    }, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(modules, ensure_ascii=False))
elif sys.argv[1] == "read":
    for spec in sys.argv[2:]:
        name, start, end = spec.rsplit(":", 2)
        path = ROOT / name
        lines = path.read_text(encoding="utf-8-sig").splitlines()
        start, end = int(start), min(int(end), len(lines))
        row = {"path": name, "start": start, "end": end, "sha256": digest(path)}
        with READS.open("a", encoding="utf-8") as out:
            out.write(json.dumps(row, ensure_ascii=False) + "\n")
        print("\n" + name)
        for i in range(start - 1, end):
            print(f"{i + 1}: {lines[i]}")
elif sys.argv[1] == "verify":
    before = json.loads(MANIFEST.read_text(encoding="utf-8"))["files"]
    changed = [x["path"] for x in before if not (ROOT / x["path"]).is_file()
               or digest(ROOT / x["path"]) != x["sha256"]]
    added = sorted(set(source_files()) - {x["path"] for x in before})
    result = {"changed_since_snapshot": changed, "added_since_snapshot": added}
    (HERE / "SOURCE_DRIFT.json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, ensure_ascii=False))
