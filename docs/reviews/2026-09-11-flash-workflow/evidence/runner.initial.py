"""One bounded product walkthrough; no credential files or automatic retries."""
import hashlib
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
PYTHON = REPO / "target/flash-workflow-env/Scripts/python.exe"
BINARY = REPO / "target/debug/agent-tui.exe"
DOMAINS = ["existing", "missing", "missing_document", "external", "encoded", "fragment", "nested", "unicode", "mixed"]

TESTS = r'''import importlib.util
import sys
from pathlib import Path
import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location("doc_consistency", ROOT / "scripts/doc_consistency.py")
checker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(checker)

@pytest.mark.parametrize("domain", ["existing", "missing", "missing_document", "external", "encoded", "fragment", "nested", "unicode", "mixed"])
def test_link_domain(domain, tmp_path):
    (tmp_path / "docs").mkdir()
    (tmp_path / "docs/sub").mkdir()
    for name in ["exists.md", "space name.md", "中文.md"]:
        (tmp_path / "docs" / name).write_text("ok", encoding="utf-8")
    page = "docs/index.md"
    body, expected = {
        "existing": ("[ok](exists.md)", []),
        "missing": ("[bad](absent.md)", ["docs/index.md: broken link -> absent.md"]),
        "missing_document": ("", ["live document missing: docs/index.md"]),
        "external": ("[a](https://example.test/a) [b](http://example.test) [c](mailto:test@example.test)", []),
        "encoded": ("[ok](space%20name.md)", []),
        "fragment": ("[ok](exists.md#section) [local](#section)", []),
        "nested": ("[ok](../exists.md)", []),
        "unicode": ("[ok](中文.md)", []),
        "mixed": ("[ok](exists.md) [bad](missing.md) [web](https://example.test)", ["docs/index.md: broken link -> missing.md"]),
    }[domain]
    if domain == "nested":
        page = "docs/sub/index.md"
    if domain != "missing_document":
        (tmp_path / page).write_text(body, encoding="utf-8")
    checker.ROOT = str(tmp_path)
    checker.LIVE_DOCS = [page]
    actual = []
    checker.check_links(actual)
    assert actual == expected
'''

PROMPTS = {
    "a": """Refactor the real Python documentation checker in this repository. Move Markdown link validation out of scripts/doc_consistency.py into scripts/doc_links.py with public function check_markdown_links(root, relative_paths, violations). The old check_links(violations) must delegate to that function using its current ROOT and LIVE_DOCS. Preserve all existing behavior and all unrelated checks. Change only those two source files, do not edit tests or create a completion marker. Read the source and tests before editing; run the provided python.pytest verification recipe (discover/load verify.run if needed), inspect git.diff, and finish with a concise explanation of changes and actual test results. Do not run shell commands. Python tests are installed and the host owns the interpreter path.""",
    "b": """Review and verify the existing scripts/doc_consistency.py link checker without changing any source or tests. The provided pytest suite covers nine domains: existing, missing, missing_document, external, encoded, fragment, nested, unicode, mixed. Read the relevant implementation and test definitions, execute the trusted python.pytest recipe via verify.run (discover/load if needed), and write validation.json with keys domains (mapping each of the nine exact ids to PASS or FAIL) and evidence (a short account of the actual test result). Do not invent a pass if execution fails. Only validation.json may be written. Finish with a concise review; no task completion declaration is authorized.""",
    "c": """Audit the long diagnostic output for this real documentation checker and preserve this exact directive when continuing. First read only README.md and tests/test_links.py, then continue your work in later model rounds. Run the trusted python.pytest recipe via verify.run to produce the diagnostic log (discover/load tools if needed). Its output is deliberately over 3 MiB, so use the returned artifact reference and bounded artifact.read pages rather than repeating the test or reading the full output. The final marker is beyond line 30000. Write summary.json with final_marker (the exact text after FINAL_MARKER=) and domains_passed (the number of passing link-domain cases). Only summary.json may be written; never change source/tests. After a restore, do not rerun the diagnostic if its output artifact already exists: inspect the stored evidence and continue reading it. Final constraint: preserve the original nine-domain checker behavior, and never claim task closure; provide output for operator review.""",
}

def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def write_json(path, data):
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False), encoding="utf-8")

def git(root, *args):
    return subprocess.check_output(["git", *args], cwd=root, text=True, encoding="utf-8").strip()

def source_manifest():
    paths = subprocess.check_output(["git", "ls-files", "-co", "--exclude-standard", "-z"], cwd=REPO).decode().split("\0")
    selected = sorted({p for p in paths if p and ((p.startswith("crates/") and "/src/" in p and p.endswith(".rs")) or p in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml") or (p.startswith("crates/") and p.count("/") == 2 and p.endswith("Cargo.toml")))})
    return {p: sha(REPO / p) for p in selected}

def seed(root, case, marker):
    root.mkdir(parents=True, exist_ok=False)
    (root / "scripts").mkdir()
    (root / "tests").mkdir()
    source = REPO / "scripts/doc_consistency.py"
    (root / "scripts/doc_consistency.py").write_bytes(source.read_bytes())
    (root / "tests/test_links.py").write_text(TESTS, encoding="utf-8")
    (root / "pytest.ini").write_text("[pytest]\naddopts = -v -s -p no:cacheprovider\n", encoding="utf-8")
    (root / "README.md").write_text("# Documentation checker walkthrough\nThe source is copied from context-agent-prototype/scripts/doc_consistency.py.\nThe host exposes the installed Python pytest suite as verification recipe python.pytest.\nDo not edit tests. Nine behavior domains are checked in tests/test_links.py.\n", encoding="utf-8")
    (root / ".gitignore").write_text(".focus-agent/\n__pycache__/\n.pytest_cache/\n", encoding="utf-8")
    if case == "c":
        (root / "tests/test_long_output.py").write_text(
            "def test_diagnostic_output():\n"
            "    for number in range(1, 32001):\n"
            "        print(f'diagnostic {number:05d} ' + 'documentation check detail ' * 4)\n"
            f"    print('FINAL_MARKER={marker}')\n", encoding="utf-8")
    git(root, "init", "--quiet")
    git(root, "add", ".")
    git(root, "-c", "user.name=Local Walkthrough", "-c", "user.email=walkthrough@invalid.local", "commit", "-qm", "Fixed walkthrough start")
    return {"commit": git(root, "rev-parse", "HEAD"), "files": {str(p.relative_to(root)): sha(p) for p in root.rglob("*") if p.is_file() and ".git" not in p.parts}}

def grants(case):
    expires = int(time.time() * 1000) + 30 * 60 * 1000
    return [
        {"id": "walkthrough-output", "risk": "WorkspaceWrite", "target": {"workspace_path_prefix": {"a":"scripts", "b":"validation.json", "c":"summary.json"}[case]}, "constraint": {"max_content_bytes": 32768}, "expires_at_ms": expires},
        {"id": "walkthrough-pytest", "risk": "ProcessExecution", "target": {"exec_argv_prefix": [str(PYTHON), "-m", "pytest"]}, "constraint": {"max_runs": 4}, "expires_at_ms": expires},
    ]

def summarize(rows, journal_rows):
    # Journal is authoritative for late shutdown/cancellation events; the
    # headless output stream intentionally closes before runtime shutdown.
    events = [r.get("event", {}) for r in journal_rows]
    if not events:
        events = [r.get("event", {}) for r in rows]
    decision = [e for e in events if e.get("type")=="model_used"]
    maintenance = [e["report"] for e in events if e.get("type")=="context_maintained"]
    tools = [e["output"] for e in events if e.get("type")=="tool_finished"]
    return {"event_counts": {name: sum(e.get("type")==name for e in events) for name in ["model_started", "model_used", "tool_started", "tool_finished", "turn_completed", "turn_cancelled", "runtime_restored", "checkpoint_durable", "recovery_required"]},
            "decision": {"calls": len(decision), **{k:sum(d.get(k,0) or 0 for d in decision) for k in ["input_tokens", "output_tokens", "cached_input_tokens", "attempts", "retries"]}},
            "maintenance": {"passes":len(maintenance), "compactions":sum(len(d.get("compactions",[])) for d in maintenance), **{k:sum(d.get(k,0) or 0 for d in maintenance) for k in ["compaction_input_tokens", "compaction_output_tokens", "deferred_folds"]}},
            "tools": [{"name":t.get("tool_name"), "ok":t.get("ok"), "artifact_ref":t.get("artifact_ref"), "metadata":t.get("metadata")} for t in tools],
            "session_end": next((r for r in reversed(rows) if r.get("kind")=="session_end"), None)}

def run_segment(root, case, segment, env, key, rounds, timeout, restore=False):
    out = root.parent / segment
    out.mkdir(exist_ok=False)
    grant_path = out / "grants.json"
    write_json(grant_path, grants(case))
    args = [str(BINARY), f"--grant-file={grant_path}", f"--max-rounds={rounds}", f"--timeout-secs={timeout}", "--context=dynamic"]
    if restore:
        args += ["--restore=latest", "--continue"]
        prompt = ""
    else:
        args += ["--work", "--prompt=-"]
        prompt = PROMPTS[case]
    args += [str(root)]
    local_env = dict(env, OPENAI_RETRY_METRICS_FILE=str(out / "retries.jsonl"))
    print(json.dumps({"starting":segment, "max_rounds":rounds, "timeout_secs":timeout}), flush=True)
    started = time.perf_counter()
    proc = subprocess.Popen(args, cwd=root, env=local_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8", errors="replace", creationflags=getattr(subprocess,"CREATE_NO_WINDOW",0))
    def collect(pipe, destination, is_events):
        with destination.open("w", encoding="utf-8") as output:
            for line in pipe:
                clean = line.replace(key, "[credential removed]")
                output.write(clean)
                output.flush()
                if is_events and clean.strip():
                    try:
                        event = json.loads(clean).get("event", {})
                        if isinstance(event, dict) and event.get("type") in ("model_used", "turn_commit_failed"):
                            print(json.dumps({"segment":segment,"event":event}), flush=True)
                    except (ValueError, TypeError):
                        pass
    threads = [threading.Thread(target=collect, args=(proc.stdout,out/"events.jsonl",True)), threading.Thread(target=collect, args=(proc.stderr,out/"stderr.txt",False))]
    for thread in threads: thread.start()
    proc.stdin.write(prompt)
    proc.stdin.close()
    forced = False
    try:
        code = proc.wait(timeout=timeout+45)
    except subprocess.TimeoutExpired:
        forced = True
        subprocess.run(["taskkill","/PID",str(proc.pid),"/T","/F"], capture_output=True, check=False)
        code = proc.wait(timeout=15)
    for thread in threads: thread.join(timeout=10)
    elapsed = round((time.perf_counter()-started)*1000, 2)
    rows = []
    for line in (out/"events.jsonl").read_text(encoding="utf-8").splitlines():
        if line.strip(): rows.append(json.loads(line))
    run_id = next((r.get("run_id") for r in rows if r.get("run_id")), None)
    journal = []
    for path in (root/".focus-agent/traces").glob("*.jsonl"):
        for line in path.read_text(encoding="utf-8").splitlines():
            if line.strip():
                row = json.loads(line)
                if not run_id or row.get("run_id") == run_id: journal.append(row)
    report = {"segment":segment,"argv":args,"exit_code":code,"wall_ms":elapsed,"forced_tree_stop":forced,"run_id":run_id,"metrics":summarize(rows,journal)}
    write_json(out/"result.json",report)
    print(json.dumps({"finished":segment,"exit":code,"wall_ms":elapsed,"metrics":report["metrics"]["decision"]}),flush=True)
    return report

def verify(root, case, marker, start):
    env = dict(os.environ, PYTHONDONTWRITEBYTECODE="1")
    env.pop("DEEPSEEK_API_KEY",None); env.pop("OPENAI_API_KEY",None)
    checked = subprocess.run([str(PYTHON),"-m","pytest","tests/test_links.py","-q"],cwd=root,env=env,capture_output=True,text=True,timeout=30)
    source = root / "scripts/doc_consistency.py"
    result = {"nine_domain_tests_exit":checked.returncode,"nine_domain_tests":checked.stdout+checked.stderr,"tests_unchanged":all(sha(root/path)==digest for path,digest in start["files"].items() if path.startswith("tests/") or path=="pytest.ini"),"diff":git(root,"diff","--stat"),"status":git(root,"status","--short")}
    if case == "a":
        helper = root/"scripts/doc_links.py"
        result["helper_exists"] = helper.is_file()
        # Verify actual delegation, not just the presence of a function name.
        probe = "import sys; sys.path.insert(0,'scripts'); import doc_consistency as c, doc_links as l; calls=[]; f=lambda *args:calls.append(args); l.check_markdown_links=f; c.check_markdown_links=f; v=[]; c.check_links(v); assert calls and calls[0][0]==c.ROOT and calls[0][1]==c.LIVE_DOCS and calls[0][2] is v"
        check = subprocess.run([str(PYTHON),"-c",probe],cwd=root,env=env,capture_output=True,text=True,timeout=10)
        result["delegation_verified"] = check.returncode == 0
        result["delegation_error"] = check.stderr
        result["passed"] = checked.returncode==0 and result["tests_unchanged"] and helper.is_file() and check.returncode==0
    elif case == "b":
        try: payload=json.loads((root/"validation.json").read_text(encoding="utf-8"))
        except (OSError,ValueError): payload=None
        result["output"] = payload
        result["source_unchanged"] = sha(source)==start["files"]["scripts/doc_consistency.py"]
        result["passed"] = checked.returncode==0 and result["tests_unchanged"] and result["source_unchanged"] and isinstance(payload,dict) and payload.get("domains")==dict.fromkeys(DOMAINS,"PASS")
    else:
        try: payload=json.loads((root/"summary.json").read_text(encoding="utf-8"))
        except (OSError,ValueError): payload=None
        result["output"] = payload
        result["source_unchanged"] = sha(source)==start["files"]["scripts/doc_consistency.py"]
        result["passed"] = checked.returncode==0 and result["tests_unchanged"] and result["source_unchanged"] and isinstance(payload,dict) and payload.get("final_marker")==marker and payload.get("domains_passed")==9
    return result

def main():
    key = os.environ.get("DEEPSEEK_API_KEY") or sys.stdin.readline(4097).strip()
    if not key or len(key)>4096: raise SystemExit("Process-local DeepSeek credential required; nothing sent")
    stamp = str(time.time_ns())
    run = REPO / "target/flash-workflow" / stamp
    run.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ)
    for name in ["DEEPSEEK_API_KEY","OPENAI_API_KEY","OPENAI_TEMPERATURE","AGENT_DEMO","OPENAI_RETRY_METRICS_FILE"]: env.pop(name,None)
    env.update({"OPENAI_API_KEY":key,"OPENAI_BASE_URL":"https://api.deepseek.com","OPENAI_MODEL":"deepseek-flash","OPENAI_API_PROTOCOL":"responses","OPENAI_PROMPT_CACHE_MODE":"provider_default","OPENAI_RESPONSES_REASONING_EFFORT":"none","OPENAI_CONTEXT_WINDOW":"128000","OPENAI_MAX_OUTPUT_TOKENS":"2048","AGENT_PYTHON":str(PYTHON),"PYTHONDONTWRITEBYTECODE":"1"})
    sources = source_manifest()
    manifest = {"schema":"flash-workflow-walkthrough/v1","head":git(REPO,"rev-parse","HEAD"),"binary_sha256":sha(BINARY),"source_hashes":sources,"runner_sha256":sha(Path(__file__)),"prompts":PROMPTS,"run_dir":str(run),"cases":{}}
    write_json(run/"manifest.json",manifest)
    print(json.dumps({"run_dir":str(run),"source_files":len(sources)}),flush=True)
    for case in ["a","b","c"]:
        root=run/case/"workspace"
        marker="DOC-LINK-TAIL-"+hashlib.sha256((stamp+case).encode()).hexdigest()[:20]
        start=seed(root,case,marker)
        write_json(root.parent/"start.json",start)
        (root.parent/"prompt.txt").write_text(PROMPTS[case],encoding="utf-8")
        baseline=subprocess.run([str(PYTHON),"-m","pytest","tests/test_links.py","-q"],cwd=root,capture_output=True,text=True,timeout=30)
        if baseline.returncode: raise SystemExit("Fixed baseline checks failed before API calls: "+baseline.stdout+baseline.stderr)
        segments=[]
        if case=="c":
            segments.append(run_segment(root,case,"c-read-yield",env,key,1,120))
            if segments[-1]["exit_code"]==2:
                segments.append(run_segment(root,case,"c-cancel",env,key,4,1,True))
                if not segments[-1]["forced_tree_stop"]:
                    segments.append(run_segment(root,case,"c-resume",env,key,12,300,True))
        else:
            segments.append(run_segment(root,case,case+"-main",env,key,12 if case=="a" else 8,300 if case=="a" else 240))
        result=verify(root,case,marker,start)
        write_json(root.parent/"verification.json",result)
        (root.parent/"final.diff").write_text(git(root,"diff"),encoding="utf-8")
        manifest["cases"][case]={"segments":segments,"verification":result}
        write_json(run/"manifest.json",manifest)
        print(json.dumps({"case":case,"verified":result["passed"]}),flush=True)
    manifest["sources_unchanged"]=all(sha(REPO/path)==digest for path,digest in sources.items())
    write_json(run/"manifest.json",manifest)
    write_json(HERE/"latest.json",{"run_dir":str(run),"head":manifest["head"],"sources_unchanged":manifest["sources_unchanged"],"case_results":{c:d["verification"]["passed"] for c,d in manifest["cases"].items()}})
    print(json.dumps({"complete":str(run),"sources_unchanged":manifest["sources_unchanged"]}),flush=True)

if __name__=="__main__": main()
