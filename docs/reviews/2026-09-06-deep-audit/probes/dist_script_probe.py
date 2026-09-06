#!/usr/bin/env python3
"""Execute the exact repository dist.sh in temporary fixtures with a stub cargo.
This tests packaging/control flow, NOT Rust compilation or real binary quality.
"""
from __future__ import annotations
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "source-excerpts/scripts/dist.sh"
EXPECTED_BLOB = "b3a3f9a6326df15054cd55e430387c3aeec75008"
STUB = '''#!{python}
import json, os, pathlib, sys
args = sys.argv[1:]
with open("cargo-calls.jsonl", "a") as f: f.write(json.dumps(args)+"\\n")
if os.environ.get("STUB_CARGO_FAIL"): sys.exit(17)
out = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", "target"))
if "--target-dir" in args: out = pathlib.Path(args[args.index("--target-dir")+1])
name = args[args.index("--bin")+1]
(out/"release").mkdir(parents=True, exist_ok=True)
(out/"release"/name).write_text("FRESH "+name+"\\n")
'''

def one_case(case: str) -> dict:
    with tempfile.TemporaryDirectory(prefix="audit-dist-") as directory:
        root = pathlib.Path(directory)
        (root/"scripts").mkdir()
        shutil.copyfile(SOURCE, root/"scripts/dist.sh")
        (root/"Cargo.toml").write_text('[workspace.package]\nversion = "0.1.0"\n')
        stubdir = root/"stub-bin"; stubdir.mkdir()
        stub = stubdir/"cargo"; stub.write_text(STUB.format(python=sys.executable)); stub.chmod(0o700)
        env = dict(os.environ)
        env.pop("CARGO_TARGET_DIR", None)
        env.pop("STUB_CARGO_FAIL", None)
        env["PATH"] = str(stubdir) + os.pathsep + env.get("PATH", "")
        args = ["bash", "scripts/dist.sh"]
        if case == "custom_target_copies_stale_binary":
            (root/"old-target/release").mkdir(parents=True)
            (root/"old-target/release/agent-tui").write_text("STALE agent-tui\n")
            args.append("old-target")
        elif case == "previous_dist_files_survive":
            (root/"dist/0.1.0").mkdir(parents=True)
            (root/"dist/0.1.0/agent-context-service").write_text("STALE helper\n")
        elif case == "bash_build_failure_stops_packaging":
            env["STUB_CARGO_FAIL"] = "1"
        else:
            raise ValueError(case)
        result = subprocess.run(args,cwd=root,env=env,text=True,capture_output=True,timeout=10)
        out = root/"dist/0.1.0"
        files = {p.name:p.read_text() for p in out.iterdir() if p.is_file()} if out.exists() else {}
        calls_path = root/"cargo-calls.jsonl"
        calls = [json.loads(line) for line in calls_path.read_text().splitlines()] if calls_path.exists() else []
        return {"case":case,"exit_code":result.returncode,"cargo_calls":calls,
                "output_files":files,"stdout":result.stdout,"stderr":result.stderr,
                "real_cargo_invoked":False}

def main() -> None:
    data = SOURCE.read_bytes()
    blob = hashlib.sha1(b"blob "+str(len(data)).encode()+b"\0"+data).hexdigest()
    if blob != EXPECTED_BLOB:
        raise SystemExit("Refusing to test a script that does not match the reviewed source blob.")
    cases = [one_case(name) for name in ("custom_target_copies_stale_binary", "previous_dist_files_survive", "bash_build_failure_stops_packaging")]
    assert cases[0]["exit_code"] == 0
    assert cases[0]["output_files"]["agent-tui"] == "STALE agent-tui\n"
    assert cases[1]["exit_code"] == 0
    assert cases[1]["output_files"]["agent-context-service"] == "STALE helper\n"
    assert "agent-context-service" in cases[1]["output_files"]["SHA256SUMS"]
    assert cases[2]["exit_code"] == 17 and not cases[2]["output_files"]
    print(json.dumps({"test_kind":"EXACT_REPOSITORY_SCRIPT_WITH_STUB_BUILDER", "source_git_blob_sha1":blob,
                      "real_rust_compilation":False, "tests_executed":3, "cases":cases},ensure_ascii=False,indent=2))

if __name__ == "__main__":
    main()
