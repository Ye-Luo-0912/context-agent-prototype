"""Controller-owned continuous load for one V5 task workspace.

The controller keeps the repository and feedback stream alive while the model
works. It records failures instead of turning them into a new task or erasing
earlier evidence. It imports only the independent oracle and invokes the
candidate in short, bounded child processes.
"""
import argparse
import json
import os
from pathlib import Path
import random
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse

from oracle import check_archive, check_restored, invoke, repository_cut, summary


def write_json(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def request(index, generation):
    tenant = "tenant-%02d" % (index % 8)
    environment = "prod" if index % 3 else "staging"
    return dict(tenant=tenant, environment=environment, key="release-%06d" % index,
                requirements={"pkg-%03d" % (index % 120): {"min": 1 + index % 3,
                                                              "max": 2 + index % 3}},
                expected_generation=generation)


class Receiver:
    def __init__(self, out):
        self.out = Path(out)
        self.audit = []
        self.lock = threading.Lock()
        parent = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_GET(self):
                with parent.lock:
                    parent.audit.append(dict(method="GET", path=self.path))
                    rows = list(parent.audit)
                body = json.dumps(rows, ensure_ascii=False).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length)
                record = dict(method="POST", path=self.path, body=json.loads(body.decode("utf-8")))
                with parent.lock:
                    parent.audit.append(record)
                    count = sum(row["method"] == "POST" for row in parent.audit)
                write_json(parent.out / "receiver-audit.json", parent.audit)
                # The first publication loses its ACK after the receiver has
                # accepted it. The next retry must query identity before POST.
                if count == 1:
                    self.close_connection = True
                    return
                response = json.dumps(record["body"], ensure_ascii=False).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Length", str(len(response)))
                self.end_headers()
                self.wfile.write(response)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    @property
    def url(self):
        return "http://127.0.0.1:%d/"

    def start(self):
        self.thread.start()
        return self.url % self.server.server_port

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(5)
        write_json(self.out / "receiver-audit.json", self.audit)


def candidate_process(work, operation, kwargs, *, root=None, timeout=45):
    command = [sys.executable, "-B", str(Path(__file__).with_name("invoke.py")), str(work)]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, encoding="utf-8")
    try:
        process.stdin.write(json.dumps({"op": operation, "root": str(root) if root is not None else None,
                                        "kwargs": kwargs}) + "\n")
        process.stdin.close()
        process.stdin = None
        stdout, stderr = process.communicate(timeout=timeout)
        value = json.loads(stdout.strip()) if stdout.strip() else None
        return dict(returncode=process.returncode, value=value, stderr=stderr[-3000:])
    except subprocess.TimeoutExpired:
        process.kill()
        stdout, stderr = process.communicate(timeout=10)
        return dict(returncode=process.returncode, value=None, stderr=(stderr + stdout)[-3000:], timeout=True)


def run(stage, seconds, seed, *, out_name="continuous-load", root_override=None):
    stage = Path(stage).resolve()
    work = stage / "workspace"
    out = stage / out_name
    out.mkdir(parents=True, exist_ok=False)
    feedback = work / "runtime-feedback"
    feedback.mkdir(exist_ok=True)
    rng = random.Random(seed)
    receiver = Receiver(out)
    receiver_url = receiver.start()
    root = Path(root_override).resolve() if root_override else out / "repository"
    events_path = feedback / "events.jsonl"
    started = time.time()
    rows = []
    passed = failed = 0
    generation_by_scope = {}
    archives = 0
    crashes = []
    source_identity = None
    status = "RUNNING"
    try:
        first = request(0, None)
        first["key"] = "seed"
        result = candidate_process(work, "install", first, root=root)
        if not result.get("value", {}).get("ok"):
            raise RuntimeError("base application install failed: " + repr(result))
        for index in range(1, 1_000_000):
            if time.time() >= started + seconds:
                break
            scope = ("tenant-%02d" % (index % 8), "prod" if index % 3 else "staging")
            generation = generation_by_scope.get(scope, 0)
            operation = "install" if index % 4 else "active"
            details = dict(batch=index, operation=operation, scope=scope)
            if operation == "install":
                payload = request(index, generation)
                result = candidate_process(work, operation, payload, root=root)
                details["candidate"] = result
                if result.get("value", {}).get("ok"):
                    generation_by_scope[scope] = generation + 1
            else:
                result = candidate_process(work, operation,
                                           dict(tenant=scope[0], environment=scope[1]), root=root)
                details["candidate"] = result

            # A publication with the first ACK deliberately lost.
            if index == 3:
                details["publication"] = candidate_process(
                    work, "publish", dict(tenant="tenant-00", environment="staging",
                                           key="seed", url=receiver_url), root=root)
            if index % 2 == 0:
                archive = out / ("snapshot-%06d.zip" % index)
                result = candidate_process(work, "backup_live", dict(root=str(root), archive=str(archive)))
                details["backup"] = result
                if result.get("value", {}).get("ok") and archive.exists():
                    archives += 1
                    try:
                        descriptor, members = repository_cut(root)
                        expected = summary(descriptor, members)
                        check_archive(archive, members, expected)
                        restore_dir = out / ("restore-%06d" % index)
                        restored = candidate_process(work, "restore_live",
                                                    dict(archive=str(archive), destination=str(restore_dir)))
                        details["restore"] = restored
                        if restored.get("value", {}).get("ok"):
                            check_restored(restore_dir, members, expected)
                            passed += 1
                        else:
                            failed += 1
                    except Exception as error:
                        details["oracle_error"] = f"{type(error).__name__}: {error}"
                        failed += 1
                else:
                    failed += 1
            # Real crash boundary: child must exit 74, then an ordinary retry
            # must either produce the same valid archive or a typed refusal.
            if index == 5:
                crash_archive = out / "crash-before-publish.zip"
                crash = candidate_process(work, "backup_live",
                                          dict(root=str(root), archive=str(crash_archive),
                                               crash_at="before_publish"))
                crashes.append(dict(index=index, crash=crash,
                                    destination_present=crash_archive.exists()))
            try:
                descriptor, members = repository_cut(root)
                source_identity = summary(descriptor, members)
            except Exception as error:
                details["source_oracle_error"] = f"{type(error).__name__}: {error}"
            details.update(passed=passed, failed=failed, archives=archives,
                           elapsed=round(time.time() - started, 3))
            rows.append(details)
            with events_path.open("a", encoding="utf-8") as stream:
                stream.write(json.dumps(details, ensure_ascii=False, sort_keys=True) + "\n")
            write_json(feedback / "latest.json", dict(status="RUNNING", batch=index,
                                                       passed=passed, failed=failed,
                                                       archives=archives, crashes=crashes[-3:],
                                                       latest=details,
                                                       instruction="Inspect this report, preserve prior behavior, and repair the next concrete failure."))
            time.sleep(0.03)
        status = "PASS" if archives and failed == 0 else "INCOMPLETE"
    except BaseException as error:
        status = "FAILED"
        rows.append(dict(controller_error=f"{type(error).__name__}: {error}"))
        write_json(feedback / "latest.json", dict(status=status, error=rows[-1],
                                                   passed=passed, failed=failed))
    finally:
        receiver.close()
        write_json(out / "receipt.json", dict(status=status, seed=seed, target_seconds=seconds,
                                              elapsed_seconds=time.time() - started, batches=len(rows),
                                              passed=passed, failed=failed, archives=archives,
                                              crashes=crashes, source_summary=source_identity,
                                              receiver_audit=receiver.audit))
    return 0 if status == "PASS" else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("--seconds", type=int, default=300)
    parser.add_argument("--seed", type=int, default=20260921)
    parser.add_argument("--out-name", default="continuous-load")
    parser.add_argument("--root")
    args = parser.parse_args()
    raise SystemExit(run(args.stage, args.seconds, args.seed,
                         out_name=args.out_name, root_override=args.root))
