"""Controller-owned continuous load for one V5 task workspace.

The controller keeps the repository, the feedback stream and the receiver alive
while the model works. It records failures instead of turning them into a new
task or erasing earlier evidence, and it never imports candidate helpers to
define an expected answer.

Shape of the load (review F03/F04/F05/F07/F08):

* independent writer workers install concurrently in barrier-synchronized
  rounds, so write/write overlap is a measured interval fact rather than a
  thread count;
* a reader/GC executor mutates on its own, outside the model's turns;
* a verification executor restores and oracle-checks each archive while the
  next round keeps writing;
* every candidate call that reads the source happens inside one quiesced window
  with a source fingerprint taken immediately before and after it;
* failures stay in a bounded unresolved list until the same obligation is
  proven resolved, and an acceptance window is bound to the candidate digest;
* planned faults carry planned/fired/observed/verdict rows and are never
  reported as triggered when the trigger did not fire.
"""
import argparse
import hashlib
import json
from pathlib import Path
import queue
import random
import sqlite3
import subprocess
import sys
import threading
import time
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qsl, urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))

import workload  # noqa: E402
from oracle import (check_archive, check_restored, repository_cut,  # noqa: E402
                    source_fingerprint, summary)

SUMMARY_KEYS = {"cut_id", "scopes", "receipts", "manifests", "objects", "outbox"}
FAILURE_LIST_LIMIT = 16
INTERVAL_SAMPLE_LIMIT = 8192


def write_json(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


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
        return dict(returncode=process.returncode, value=None,
                    stderr=(stderr + stdout)[-3000:], timeout=True)


def candidate_digest(work):
    """Identity of the frozen candidate version an acceptance window refers to."""
    work = Path(work)
    parts = []
    for relative in ("app/live_backup.py", "app/tests/test_live_backup.py"):
        path = work / relative
        parts.append(relative.encode() + b"\0" + (path.read_bytes() if path.exists() else b"<absent>"))
    return hashlib.sha256(b"\1".join(parts)).hexdigest()


def authority_state(root):
    """Resume facts read from the authority database, never from memory."""
    root = Path(root)
    database = root / "repo.sqlite"
    if not database.exists():
        return dict(generations={}, receipts={}, max_slot=0, receipt_count=0)
    connection = sqlite3.connect(f"{database.resolve().as_uri()}?mode=ro", uri=True, isolation_level=None)
    try:
        connection.execute("PRAGMA query_only=1")
        generations = {}
        for tenant, environment, text in connection.execute(
                "SELECT tenant,environment,receipt_json FROM current"):
            generations[f"{tenant}/{environment}"] = json.loads(text).get("generation", 0)
        receipts = {}
        for tenant, environment, count in connection.execute(
                "SELECT tenant,environment,COUNT(*) FROM receipts GROUP BY tenant,environment"):
            receipts[f"{tenant}/{environment}"] = count
        keys = [row[0] for row in connection.execute("SELECT key FROM receipts")]
    finally:
        connection.close()
    slots = [int(key.split("-")[1]) for key in keys
             if key.startswith("release-") and key.split("-")[1].isdigit()]
    return dict(generations=generations, receipts=receipts,
                max_slot=max(slots, default=0), receipt_count=len(keys))


class QuiesceGate:
    """Mutating executors register; the coordinator takes an exclusive window.

    ``working`` blocks while a window is open, so a quiesced window provably has
    no writer, no reader/GC mutation and no candidate source read inside it.
    """

    def __init__(self):
        self._condition = threading.Condition()
        self._active = 0
        self._exclusive = False

    @contextmanager
    def working(self):
        with self._condition:
            while self._exclusive:
                self._condition.wait()
            self._active += 1
        try:
            yield
        finally:
            with self._condition:
                self._active -= 1
                self._condition.notify_all()

    @contextmanager
    def quiesced(self):
        with self._condition:
            self._exclusive = True
            while self._active:
                self._condition.wait()
        try:
            yield
        finally:
            with self._condition:
                self._exclusive = False
                self._condition.notify_all()

    def snapshot(self):
        with self._condition:
            return dict(active=self._active, exclusive=self._exclusive)


class SlotAllocator:
    """Monotonic slot allocation; a slot is never reused inside a campaign."""

    def __init__(self, start: int, cap: int):
        self._lock = threading.Lock()
        self._next = start
        self._cap = cap
        self.allocated = 0

    def take(self):
        with self._lock:
            if self._next > self._cap:
                return None
            slot = self._next
            self._next += 1
            self.allocated += 1
            return slot

    def peek(self):
        with self._lock:
            return min(self._next, self._cap)

    @property
    def exhausted(self):
        with self._lock:
            return self._next > self._cap


class FailureLedger:
    """Bounded unresolved failures; only a matching success clears an entry."""

    def __init__(self, limit=FAILURE_LIST_LIMIT):
        self._lock = threading.Lock()
        self._rows = {}
        self._order = []
        self.limit = limit
        self.overflow = 0
        self.total = 0
        self.resolved = 0

    def fail(self, obligation, *, slot, scope, operation, detail, reproduce, digest):
        with self._lock:
            self.total += 1
            row = self._rows.get(obligation)
            if row is None:
                row = dict(obligation=obligation, first_slot=slot, count=0)
                self._rows[obligation] = row
            row.update(last_slot=slot, scope=scope, operation=operation, detail=str(detail)[:2000],
                       reproduce=reproduce, candidate_digest=digest,
                       last_seen_epoch=round(time.time(), 3))
            row["count"] += 1
            if obligation in self._order:
                self._order.remove(obligation)
            self._order.append(obligation)
            while len(self._order) > self.limit:
                dropped = self._order.pop(0)
                self._rows.pop(dropped, None)
                self.overflow += 1

    def resolve(self, obligation, *, scope=None, operation=None):
        """A matching success clears the obligation; other batches never do."""
        with self._lock:
            row = self._rows.get(obligation)
            if row is None:
                return False
            if scope is not None and row.get("scope") != scope:
                return False
            if operation is not None and row.get("operation") != operation:
                return False
            self._rows.pop(obligation, None)
            self._order.remove(obligation)
            self.resolved += 1
            return True

    def rows(self):
        with self._lock:
            return [dict(self._rows[key]) for key in self._order]

    def summary(self):
        with self._lock:
            return dict(open=len(self._rows), total=self.total, resolved=self.resolved,
                        dropped_for_capacity=self.overflow, limit=self.limit)


class AcceptanceWindows:
    """Acceptance bound to the frozen candidate version, not to the campaign."""

    def __init__(self, required_seconds: float, pass_rate: float = 0.9):
        self.required_seconds = required_seconds
        self.pass_rate = pass_rate
        self.closed = []
        self.current = None
        self._lock = threading.Lock()

    def _open(self, digest, now):
        return dict(candidate_digest=digest, opened_epoch=round(now, 3), closed_epoch=None,
                    seconds=0.0, cycles=0, passed=0, failed=0, refused=0,
                    faults=dict(planned=0, observed=0), verdict="OPEN", reasons=[])

    def observe(self, digest, *, now, outcome=None):
        with self._lock:
            if self.current is None or self.current["candidate_digest"] != digest:
                if self.current is not None:
                    self._close_locked(now, superseded_by=digest)
                self.current = self._open(digest, now)
            if outcome is not None:
                self.current[outcome] = self.current.get(outcome, 0) + 1
                if outcome in ("passed", "failed"):
                    self.current["cycles"] += 1
            self.current["seconds"] = round(now - self.current["opened_epoch"], 3)
            return dict(self.current)

    def plan_fault(self, digest):
        with self._lock:
            if self.current is None or self.current["candidate_digest"] != digest:
                return None
            self.current["faults"]["planned"] += 1
            return dict(self.current)

    def note_fault(self, digest, *, observed):
        with self._lock:
            if self.current is None or self.current["candidate_digest"] != digest:
                return None
            if observed:
                self.current["faults"]["observed"] += 1
            return dict(self.current)

    def snapshot(self):
        with self._lock:
            return dict(self.current) if self.current else None

    def evaluate(self, window):
        reasons = []
        if window["seconds"] < self.required_seconds:
            reasons.append(f"window {window['seconds']}s < required {self.required_seconds}s")
        if window["cycles"] == 0:
            reasons.append("no verified archive cycle in this candidate window")
        elif window["passed"] / window["cycles"] < self.pass_rate:
            reasons.append(f"pass rate {window['passed'] / window['cycles']:.3f} < {self.pass_rate}")
        if window["faults"]["planned"] and not window["faults"]["observed"]:
            reasons.append("planned fault was never observed in this candidate window")
        return ("PASS" if not reasons else "INCOMPLETE"), reasons

    def _close_locked(self, now, *, superseded_by=None):
        self.current["closed_epoch"] = round(now, 3)
        self.current["seconds"] = round(now - self.current["opened_epoch"], 3)
        self.current["verdict"], self.current["reasons"] = self.evaluate(self.current)
        if superseded_by:
            self.current["superseded_by"] = superseded_by
        self.closed.append(self.current)
        self.current = None

    def close(self, now):
        with self._lock:
            if self.current is None:
                return None
            self._close_locked(now)
            return dict(self.closed[-1])

    def history(self):
        with self._lock:
            return [dict(row) for row in self.closed]


class FaultLedger:
    """planned / fired / observed / verdict for every fault boundary."""

    def __init__(self):
        self._rows = []
        self._lock = threading.Lock()

    def plan(self, identifier, *, slot, description):
        with self._lock:
            row = next((item for item in self._rows if item["id"] == identifier), None)
            if row is None:
                row = dict(id=identifier, slot=slot, description=description)
                self._rows.append(row)
            row.update(planned=True, fired=False, observed=False, verdict="NOT_TRIGGERED",
                       evidence=None, epoch=None)
            return dict(row)

    def record(self, identifier, *, fired, observed, verdict, evidence):
        with self._lock:
            row = next((item for item in self._rows if item["id"] == identifier), None)
            if row is None:
                row = dict(id=identifier, slot=None, description="unplanned", planned=False)
                self._rows.append(row)
            row.update(fired=fired, observed=observed, verdict=verdict, evidence=evidence,
                       epoch=round(time.time(), 3))
            return dict(row)

    def rows(self):
        with self._lock:
            return [dict(row) for row in self._rows]

    def coverage(self):
        with self._lock:
            return dict(planned=sum(1 for row in self._rows if row.get("planned")),
                        observed=sum(1 for row in self._rows if row["observed"]),
                        not_triggered=sum(1 for row in self._rows
                                          if row["verdict"] == "NOT_TRIGGERED"))


class Intervals:
    """Bounded interval evidence, used to prove real overlap."""

    def __init__(self, limit=INTERVAL_SAMPLE_LIMIT):
        self._lock = threading.Lock()
        self._rows = []
        self.limit = limit
        self.dropped = 0

    def record(self, executor, kind, slot, start, end, outcome):
        with self._lock:
            self._rows.append(dict(executor=executor, kind=kind, slot=slot,
                                   start=round(start, 6), end=round(end, 6), outcome=outcome))
            if len(self._rows) > self.limit:
                self._rows.pop(0)
                self.dropped += 1

    def _overlap(self, left_kinds, right_kinds):
        with self._lock:
            rows = list(self._rows)
        left = [row for row in rows if row["kind"] in left_kinds]
        right = [row for row in rows if row["kind"] in right_kinds]
        pairs = 0
        longest = 0.0
        for first in left:
            for second in right:
                if first["executor"] == second["executor"]:
                    continue
                span = min(first["end"], second["end"]) - max(first["start"], second["start"])
                if span > 0:
                    pairs += 1
                    longest = max(longest, span)
        return dict(pairs=pairs, longest_seconds=round(longest, 6))

    def summary(self):
        with self._lock:
            recorded = len(self._rows)
            executors = sorted({row["executor"] for row in self._rows})
            kinds = sorted({row["kind"] for row in self._rows})
        return dict(
            recorded=recorded, dropped_for_capacity=self.dropped,
            writer_writer=self._overlap(("install",), ("install",)),
            verify_writer=self._overlap(("verify",), ("install",)),
            gc_writer=self._overlap(("gc", "gc-read"), ("install",)),
            executors=executors, kinds=kinds,
        )

    def rows(self):
        with self._lock:
            return list(self._rows)


class Receiver:
    """Loopback publication receiver speaking the application's protocol.

    ``POST <base>/publish`` accepts the canonical receipt body; the first
    accepted publication deliberately loses its ACK, so a correct candidate has
    to reconcile by querying ``GET <base>/receipt?tenant=&environment=&key=``
    (200 with the original body, 404 for an unknown identity) before posting a
    new one. The audit records every request, including the lost one.
    """

    def __init__(self, out):
        self.out = Path(out)
        self.audit = []
        self.publications = {}
        self.lock = threading.Lock()
        parent = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def _json(self, status, payload_bytes):
                self.send_response(status)
                self.send_header("Content-Length", str(len(payload_bytes)))
                self.end_headers()
                self.wfile.write(payload_bytes)

            def do_GET(self):
                parsed = urlparse(self.path)
                query = dict(parse_qsl(parsed.query))
                identity = (query.get("tenant"), query.get("environment"), query.get("key"))
                with parent.lock:
                    body = parent.publications.get(identity)
                    parent.audit.append(dict(method="GET", path=self.path, identity=list(identity),
                                             known=body is not None, epoch=round(time.time(), 6)))
                if body is None:
                    self._json(404, b'{"error": "unknown publication identity"}')
                    return
                self._json(200, body)

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                raw = self.rfile.read(length)
                try:
                    payload = json.loads(raw.decode("utf-8"))
                    identity = (payload.get("tenant"), payload.get("environment"), payload.get("key"))
                except (UnicodeDecodeError, json.JSONDecodeError):
                    self._json(400, b'{"error": "unparseable publication body"}')
                    return
                with parent.lock:
                    first = not parent.publications
                    stored = parent.publications.setdefault(identity, raw)
                    ack_lost = first and not any(row.get("ack_lost") for row in parent.audit)
                    parent.audit.append(dict(method="POST", path=self.path, identity=list(identity),
                                             body=payload, duplicate=stored != raw,
                                             ack_lost=ack_lost, epoch=round(time.time(), 6)))
                write_json(parent.out / "receiver-audit.json", parent.audit)
                if ack_lost:
                    # Accepted, but the acknowledgement never reaches the caller.
                    self.close_connection = True
                    return
                self._json(200, stored)

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


class Load:
    """One continuous load window; all state is window-scoped."""

    def __init__(self, stage, out, root, *, seconds, window, writer_workers, backup_every,
                 period_floor, required_seconds, seed, resume):
        self.stage = Path(stage)
        self.work = self.stage / "workspace"
        self.out = Path(out)
        self.root = Path(root)
        self.seconds = seconds
        self.window = window
        self.slot_cap = workload.window_budget(window)
        self.writer_workers = writer_workers
        self.backup_every = backup_every
        self.period_floor = period_floor
        self.required_seconds = required_seconds
        self.seed = seed
        self.resume = resume
        self.rng = random.Random(seed)
        self.gate = QuiesceGate()
        self.intervals = Intervals()
        self.failures = FailureLedger()
        self.windows = AcceptanceWindows(required_seconds)
        self.faults = FaultLedger()
        self.receiver = Receiver(self.out)
        self.verify_queue = queue.Queue()
        self.stop = threading.Event()
        self.state_path = self.stage / "continuous-load-state.json"
        self.rows = []
        self.archives = 0
        self.verified = 0
        self.refused = 0
        self.status = "RUNNING"
        self.reasons = []
        self.stopped_by = None
        self.published_identity = None
        self.publish_attempts = 0
        self.installed = []
        self.installed_lock = threading.Lock()
        self.faulted_digests = set()
        self.authority = authority_state(self.root)
        self._write_lock = threading.Lock()
        self._cycle_lock = threading.Lock()
        self.start_barrier = threading.Barrier(writer_workers + 1)
        self.done_barrier = threading.Barrier(writer_workers + 1)

    # -- evidence helpers ---------------------------------------------------
    def feedback(self, **payload):
        failures = self.failures.rows()
        value = dict(status=self.status, failures_open=len(failures),
                     unresolved_failures=failures,
                     window=self.windows.snapshot(),
                     fault_coverage=self.faults.coverage(),
                     instruction=("Repair the unresolved failures below in priority order. "
                                  "A passing smoke test is not acceptance; the controller must "
                                  "report a passing continuous load over the frozen window."))
        value.update(payload)
        with self._write_lock:
            write_json(self.work / "runtime-feedback/latest.json", value)
        return value

    def record(self, row):
        self.rows.append(row)
        with self._write_lock, (self.work / "runtime-feedback/events.jsonl").open(
                "a", encoding="utf-8") as stream:
            stream.write(json.dumps(row, ensure_ascii=False, sort_keys=True) + "\n")

    def mark_failure(self, obligation, *, slot, scope, operation, detail, reproduce):
        self.failures.fail(obligation, slot=slot, scope=scope, operation=operation,
                           detail=detail, reproduce=reproduce,
                           digest=candidate_digest(self.work))

    def mark_success(self, obligation, *, scope=None, operation=None):
        return self.failures.resolve(obligation, scope=scope, operation=operation)

    # -- executors ----------------------------------------------------------
    def writer_worker(self, worker_id, allocator, generations, generation_lock):
        executor = f"writer-{worker_id}"
        while not self.stop.is_set():
            try:
                self.start_barrier.wait(timeout=self.seconds + 60)
            except threading.BrokenBarrierError:
                return
            if self.stop.is_set():
                return
            slot = allocator.take()
            if slot is not None:
                tenant, environment = workload.scope_for(slot)
                scope = f"{tenant}/{environment}"
                with generation_lock:
                    generation = generations.get(scope, 0)
                started = time.time()
                with self.gate.working():
                    result = candidate_process(self.work, "install",
                                               workload.request(slot, generation), root=self.root)
                finished = time.time()
                ok = bool(result.get("value", {}).get("ok"))
                self.intervals.record(executor, "install", slot, started, finished,
                                      "ok" if ok else "failed")
                if ok:
                    with generation_lock:
                        generations[scope] = generation + 1
                    # Only a committed install is announced to the coordinator:
                    # publication picks its identity from here, never from a slot
                    # that is still in flight.
                    with self.installed_lock:
                        self.installed.append(slot)
                    self.mark_success("install", scope=scope, operation="install")
                else:
                    self.mark_failure("install", slot=slot, scope=scope, operation="install",
                                      detail=result.get("value") or result.get("stderr"),
                                      reproduce=f"invoke.py install root={self.root} slot={slot}")
            try:
                self.done_barrier.wait(timeout=self.seconds + 60)
            except threading.BrokenBarrierError:
                return

    def gc_worker(self, generations):
        executor = "reader-gc"
        index = 0
        while not self.stop.is_set():
            index += 1
            scope = sorted(generations)[index % max(len(generations), 1)] if generations else "tenant-00/prod"
            tenant, environment = scope.split("/")
            started = time.time()
            with self.gate.working():
                if index % 3 == 0:
                    result = candidate_process(self.work, "gc", {}, root=self.root)
                    kind, operation = "gc", "gc"
                else:
                    result = candidate_process(self.work, "active",
                                               dict(tenant=tenant, environment=environment),
                                               root=self.root)
                    kind, operation = "gc-read", "active"
            finished = time.time()
            ok = bool(result.get("value", {}).get("ok"))
            self.intervals.record(executor, kind, index, started, finished, "ok" if ok else "failed")
            if ok:
                self.mark_success(operation, scope=scope, operation=operation)
            else:
                self.mark_failure(operation, slot=index, scope=scope, operation=operation,
                                  detail=result.get("value") or result.get("stderr"),
                                  reproduce=f"invoke.py {operation} root={self.root}")
            # jittered cadence keeps the reader/GC an independent executor
            self.stop.wait(0.01 + self.rng.random() * 0.02)

    def verify_worker(self):
        executor = "verify"
        while True:
            task = self.verify_queue.get()
            if task is None:
                return
            archive, members, expected, restore_dir, slot = task
            digest = candidate_digest(self.work)
            started = time.time()
            restored = candidate_process(self.work, "restore_live",
                                        dict(archive=str(archive), destination=str(restore_dir)))
            outcome, detail = "failed", None
            if restored.get("value", {}).get("ok"):
                try:
                    check_archive(archive, members, expected)
                    check_restored(restore_dir, members, expected)
                    outcome = "ok"
                except Exception as error:
                    detail = f"{type(error).__name__}: {error}"
            else:
                detail = restored.get("value") or restored.get("stderr")
            finished = time.time()
            self.intervals.record(executor, "verify", slot, started, finished, outcome)
            with self._cycle_lock:
                if outcome == "ok":
                    self.verified += 1
                    self.mark_success("restore", scope="archive", operation="restore_live")
                    self.windows.observe(digest, now=time.time(), outcome="passed")
                else:
                    self.mark_failure("restore", slot=slot, scope="archive",
                                      operation="restore_live", detail=detail,
                                      reproduce=f"restore archive={archive} destination={restore_dir}")
                    self.windows.observe(digest, now=time.time(), outcome="failed")
            self.record(dict(slot=slot, kind="cycle-verify", outcome=outcome, detail=detail))

    # -- one archive cycle --------------------------------------------------
    def archive_cycle(self, slot):
        archive = self.out / f"snapshot-{slot:06d}.zip"
        digest = candidate_digest(self.work)
        row = dict(slot=slot, kind="archive-cycle")
        with self.gate.quiesced():
            before = source_fingerprint(self.root)
            result = candidate_process(self.work, "backup_live",
                                       dict(root=str(self.root), archive=str(archive)), timeout=90)
            after = source_fingerprint(self.root)
            stable = before == after
            cut_evidence = dict(slot=slot, before=before, after=after, stable=stable,
                                archive=str(archive), quiesce=self.gate.snapshot())
            members = expected = None
            oracle_error = None
            if stable:
                try:
                    descriptor, members = repository_cut(self.root)
                    expected = summary(descriptor, members)
                except Exception as error:
                    oracle_error = f"{type(error).__name__}: {error}"
        ok = bool(result.get("value", {}).get("ok"))
        declared = (result.get("value") or {}).get("value")
        row.update(candidate_ok=ok, returncode=result.get("returncode"),
                   cut_evidence=cut_evidence)
        if not stable:
            self.refused += 1
            row["outcome"] = "refused_unstable_cut_window"
            self.windows.observe(digest, now=time.time(), outcome="refused")
            self.record(row)
            return row
        if not ok:
            row["outcome"] = "candidate_refused"
            row["candidate"] = result.get("value") or result.get("stderr")
            self.mark_failure("backup", slot=slot, scope="archive", operation="backup_live",
                              detail=result.get("value") or result.get("stderr"),
                              reproduce=f"backup_live root={self.root} archive={archive}")
            self.windows.observe(digest, now=time.time(), outcome="failed")
            self.record(row)
            return row
        if oracle_error is not None:
            row["outcome"] = "oracle_refused_source"
            row["oracle_error"] = oracle_error
            self.mark_failure("source-authority", slot=slot, scope="archive",
                              operation="repository_cut", detail=oracle_error,
                              reproduce=f"oracle.repository_cut({self.root})")
            self.windows.observe(digest, now=time.time(), outcome="failed")
            self.record(row)
            return row
        if not isinstance(declared, dict) or set(declared) != SUMMARY_KEYS:
            detail = f"summary keys {sorted(declared) if isinstance(declared, dict) else declared!r}"
            row.update(outcome="summary_shape", detail=detail)
            self.mark_failure("backup-summary", slot=slot, scope="archive",
                              operation="backup_live", detail=detail,
                              reproduce="SPEC.md summary contract")
            self.windows.observe(digest, now=time.time(), outcome="failed")
            self.record(row)
            return row
        if declared != expected:
            row.update(outcome="summary_mismatch", declared=declared, expected=expected)
            self.mark_failure("backup-summary", slot=slot, scope="archive",
                              operation="backup_live",
                              detail=f"declared {declared} != oracle {expected}",
                              reproduce="SPEC.md summary contract")
            self.windows.observe(digest, now=time.time(), outcome="failed")
            self.record(row)
            return row
        self.mark_success("backup", scope="archive", operation="backup_live")
        self.archives += 1
        row.update(outcome="backup_ok", summary=declared)
        self.record(row)
        self.verify_queue.put((archive, members, expected, self.out / f"restore-{slot:06d}", slot))
        return row

    # -- fault boundaries ---------------------------------------------------
    def crash_boundary(self, slot):
        identifier = "backup_crash_before_publish"
        archive = self.out / f"crash-before-publish-{slot:06d}.zip"
        self.faults.plan(identifier, slot=slot, description="backup_live crash_at=before_publish")
        with self.gate.quiesced():
            before_listing = sorted(path.name for path in self.out.glob("crash-before-publish-*"))
            crash = candidate_process(self.work, "backup_live",
                                      dict(root=str(self.root), archive=str(archive),
                                           crash_at="before_publish"), timeout=90)
            destination_present = archive.exists()
            exit_code = crash.get("returncode")
            retry = None
            if exit_code == 74:
                retry = candidate_process(self.work, "backup_live",
                                          dict(root=str(self.root), archive=str(archive)), timeout=90)
            after_listing = sorted(path.name for path in self.out.glob("crash-before-publish-*"))
        if exit_code != 74:
            self.faults.record(identifier, fired=False, observed=False, verdict="NOT_TRIGGERED",
                               evidence=dict(exit_code=exit_code,
                                             destination_present=destination_present,
                                             detail=crash.get("value") or crash.get("stderr")))
            self.mark_failure("crash_boundary", slot=slot, scope="archive",
                              operation="backup_live:crash_at",
                              detail=f"expected exit 74 from the crash boundary, saw {exit_code}",
                              reproduce=f"backup_live crash_at=before_publish archive={archive}")
            self.record(dict(slot=slot, kind="crash-boundary", verdict="NOT_TRIGGERED",
                             exit_code=exit_code, destination_present=destination_present))
            return
        retry_ok = bool(retry.get("value", {}).get("ok")) and archive.exists()
        self.faults.record(identifier, fired=True, observed=retry_ok,
                           verdict="OBSERVED" if retry_ok else "FIRED_NOT_RECOVERED",
                           evidence=dict(exit_code=exit_code, retry_ok=retry_ok,
                                         destination_present=archive.exists(),
                                         listing_before=before_listing, listing_after=after_listing))
        if retry_ok:
            self.mark_success("crash_boundary")
            self.windows.note_fault(candidate_digest(self.work), observed=True)
        else:
            self.mark_failure("crash_boundary", slot=slot, scope="archive",
                              operation="backup_live:crash_at",
                              detail=f"retry did not recover: {retry.get('value') or retry.get('stderr')}",
                              reproduce=f"backup_live archive={archive}")
        self.record(dict(slot=slot, kind="crash-boundary",
                         verdict="OBSERVED" if retry_ok else "FIRED_NOT_RECOVERED",
                         exit_code=exit_code, retry_ok=retry_ok,
                         listing_before=before_listing, listing_after=after_listing))

    def ensure_fault(self, marker):
        """One crash boundary per acceptance window, never reported untriggered."""
        digest = candidate_digest(self.work)
        if marker <= workload.CRASH_SLOT or digest in self.faulted_digests:
            return
        self.windows.observe(digest, now=time.time())
        if self.windows.plan_fault(digest) is None:
            return
        self.faulted_digests.add(digest)
        self.crash_boundary(marker)

    # -- publication --------------------------------------------------------
    def publish(self, slot, receiver_url):
        tenant, environment = workload.scope_for(slot)
        key = workload.key_for(slot)
        self.publish_attempts += 1
        started = time.time()
        with self.gate.working():
            result = candidate_process(self.work, "publish",
                                       dict(tenant=tenant, environment=environment, key=key,
                                            url=receiver_url), root=self.root)
        finished = time.time()
        ok = bool(result.get("value", {}).get("ok"))
        self.intervals.record("publication", "publish", slot, started, finished, "ok" if ok else "failed")
        if ok:
            self.published_identity = f"{tenant}/{environment}/{key}"
            # Publication is a campaign-level obligation: the receiver loses the
            # first ACK by design, so the retry with a fresh identity is the
            # matching success for it.
            self.mark_success("publish", operation="publish")
        else:
            self.mark_failure("publish", slot=slot, scope=f"{tenant}/{environment}",
                              operation="publish", detail=result.get("value") or result.get("stderr"),
                              reproduce=f"publish {tenant}/{environment}/{key} url={receiver_url}")
        self.record(dict(kind="publish", slot=slot, identity=self.published_identity, ok=ok))

    # -- lifecycle ----------------------------------------------------------
    def run(self):
        self.out.mkdir(parents=True, exist_ok=False)
        (self.work / "runtime-feedback").mkdir(exist_ok=True)
        receiver_url = self.receiver.start()
        generations = dict(self.authority["generations"])
        for scope in self.authority["receipts"]:
            generations.setdefault(scope, 0)
        generation_lock = threading.Lock()
        start_slot = self.authority["max_slot"] + 1
        if self.resume:
            state = json.loads(self.state_path.read_bytes()) if self.state_path.exists() else {}
            start_slot = max(start_slot, int(state.get("next_slot", 1)))
            self.published_identity = state.get("published_identity")
        write_json(self.out / "resume-evidence.json",
                   dict(window=self.window, resume=self.resume, slot_cap=self.slot_cap,
                        start_slot=start_slot, authority=self.authority,
                        published_identity=self.published_identity,
                        note="a resumed window continues with new keys and the generation read "
                             "from the authority database; old identities are never replayed "
                             "as new load"))
        self.record(dict(kind="window-start", window=self.window, start_slot=start_slot,
                         slot_cap=self.slot_cap, resume=self.resume,
                         authority=self.authority))
        allocator = SlotAllocator(start_slot, self.slot_cap)
        writers = [threading.Thread(target=self.writer_worker, daemon=True,
                                    args=(worker, allocator, generations, generation_lock),
                                    name=f"writer-{worker}")
                   for worker in range(self.writer_workers)]
        verifier = threading.Thread(target=self.verify_worker, daemon=True, name="verify")
        gc_thread = threading.Thread(target=self.gc_worker, daemon=True,
                                     args=(generations,), name="reader-gc")
        started = time.time()
        rounds = max(1, (self.slot_cap - start_slot + 1) // self.writer_workers + 1)
        period = max(self.seconds / rounds, self.period_floor)
        self._write_state(allocator.peek(), generations)
        try:
            verifier.start()
            gc_thread.start()
            for writer in writers:
                writer.start()
            round_index = 0
            while True:
                if time.time() - started >= self.seconds:
                    self.stopped_by = "duration"
                    break
                if allocator.exhausted:
                    self.stopped_by = "install budget"
                    break
                try:
                    self.start_barrier.wait(timeout=60)
                    self.done_barrier.wait(timeout=self.seconds + 60)
                except threading.BrokenBarrierError:
                    self.stopped_by = "barrier"
                    break
                round_index += 1
                marker = allocator.peek()
                self.ensure_fault(marker)
                if round_index % max(1, self.backup_every // self.writer_workers) == 0:
                    self.archive_cycle(marker)
                publish_slot = None
                with self.installed_lock:
                    if self.installed and self.installed[-1] > workload.PUBLISH_SLOT:
                        publish_slot = self.installed[-1]
                if (self.published_identity is None and publish_slot is not None
                        and self.publish_attempts < workload.PUBLISH_ATTEMPTS):
                    self.publish(publish_slot, receiver_url)
                self._write_state(allocator.peek(), generations)
                self.feedback(batch=round_index, slot=marker, archives=self.archives,
                              verified=self.verified, refused=self.refused,
                              overlap=self.intervals.summary(), stopped_by=self.stopped_by)
                deadline = started + (round_index + 1) * period
                if deadline > time.time():
                    time.sleep(min(deadline - time.time(), 5.0))
            self.status = "INCOMPLETE"
            self.record(dict(kind="window-stop", stopped_by=self.stopped_by,
                             rounds=round_index, elapsed=round(time.time() - started, 3)))
        except BaseException as error:
            self.status = "FAILED"
            self.reasons.append(f"controller error: {type(error).__name__}: {error}")
            self.record(dict(kind="controller-error", error=self.reasons[-1]))
            self.feedback(error=self.reasons[-1])
        finally:
            self.stop.set()
            for barrier in (self.start_barrier, self.done_barrier):
                try:
                    barrier.abort()
                except Exception:
                    pass
            for writer in writers:
                writer.join(timeout=20)
            gc_thread.join(timeout=20)
            self.verify_queue.put(None)
            verifier.join(timeout=60)
            self._finish(receiver_url)
        return self.receipt

    def _write_state(self, next_slot, generations):
        with self._write_lock:
            write_json(self.state_path, dict(next_slot=next_slot, window=self.window,
                                             generations=dict(generations),
                                             published_identity=self.published_identity,
                                             archives=self.archives, verified=self.verified))

    def _finish(self, receiver_url):
        self.receiver.close()
        self.windows.close(time.time())
        history = self.windows.history()
        last = history[-1] if history else None
        overlap = self.intervals.summary()
        open_failures = self.failures.rows()
        if self.status != "FAILED":
            self.status = "PASS" if (last is not None and last["verdict"] == "PASS") else "INCOMPLETE"
            self.reasons = list(last["reasons"]) if last is not None else ["no accepted candidate window"]
            if self.status == "PASS" and open_failures:
                # A window cannot be accepted while an obligation is still open.
                self.status = "INCOMPLETE"
                self.reasons = [f"unresolved failures: {sorted({row['obligation'] for row in open_failures})}"]
        if overlap["writer_writer"]["pairs"] == 0:
            self.status = "FAILED"
            self.reasons.append("no measured write/write overlap: the load was not concurrent")
        if overlap["verify_writer"]["pairs"] == 0:
            self.reasons.append("no measured overlap between archive verification and writers")
        receipt = dict(status=self.status, reasons=self.reasons, seed=self.seed, window=self.window,
                       target_seconds=self.seconds, stopped_by=self.stopped_by,
                       slot_cap=self.slot_cap, writer_workers=self.writer_workers,
                       batches=len(self.rows), archives=self.archives, verified=self.verified,
                       refused_batches=self.refused,
                       unresolved_failures=self.failures.rows(),
                       failure_summary=self.failures.summary(),
                       acceptance_windows=history, acceptance_window_open=self.windows.snapshot(),
                       fault_ledger=self.faults.rows(), fault_coverage=self.faults.coverage(),
                       overlap=overlap,
                       resume_evidence=dict(resume=self.resume, authority=self.authority),
                       published_identity=self.published_identity,
                       publish_attempts=self.publish_attempts,
                       committed_installs=len(self.installed),
                       receiver_requests=len(self.receiver.audit),
                       receiver_publications=len(self.receiver.publications),
                       receiver_url=receiver_url,
                       source_summary=self._source_summary())
        write_json(self.out / "receipt.json", receipt)
        write_json(self.out / "intervals.json", self.intervals.rows())
        self.feedback(status=self.status, final=True, reasons=self.reasons,
                      receipt_summary=dict(status=receipt["status"], archives=self.archives,
                                           verified=self.verified, overlap=overlap))
        self.receipt = receipt
        return receipt

    def _source_summary(self):
        try:
            descriptor, members = repository_cut(self.root)
            return summary(descriptor, members)
        except Exception as error:
            return dict(error=f"{type(error).__name__}: {error}")


def window_index_for(stage, out_name):
    """The window ordinal is derived from the existing controller outputs."""
    stage = Path(stage)
    used = 1 + sum(1 for path in stage.glob("continuous-load*") if path.is_dir()
                   and path.name != out_name)
    return used


def run(stage, seconds, seed, *, out_name="continuous-load", root_override=None, resume=False,
        window=None, writer_workers=None, backup_every=None, period_floor=0.02,
        required_seconds=None):
    stage = Path(stage).resolve()
    caps = json.loads((stage / "campaign.json").read_bytes())
    writer_workers = caps.get("writer_workers", workload.WRITER_WORKERS) if writer_workers is None else writer_workers
    backup_every = caps.get("backup_every", workload.BACKUP_EVERY) if backup_every is None else backup_every
    required_seconds = min(seconds, caps.get("target_load_seconds", seconds)) \
        if required_seconds is None else required_seconds
    window = window_index_for(stage, out_name) if window is None else window
    out = stage / out_name
    root = Path(root_override).resolve() if root_override else out / "repository"
    load = Load(stage, out, root, seconds=seconds, window=window, writer_workers=writer_workers,
                backup_every=backup_every, period_floor=period_floor,
                required_seconds=required_seconds, seed=seed, resume=resume or bool(root_override))
    receipt = load.run()
    return 0 if receipt["status"] == "PASS" else 1


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("stage", type=Path)
    parser.add_argument("--seconds", type=int, default=300)
    parser.add_argument("--seed", type=int, default=20260921)
    parser.add_argument("--out-name", default="continuous-load")
    parser.add_argument("--root")
    parser.add_argument("--window", type=int)
    parser.add_argument("--writer-workers", type=int)
    parser.add_argument("--backup-every", type=int)
    parser.add_argument("--period-floor", type=float, default=0.02)
    parser.add_argument("--required-seconds", type=float)
    parser.add_argument("--resume", action="store_true")
    args = parser.parse_args()
    raise SystemExit(run(args.stage, args.seconds, args.seed, out_name=args.out_name,
                         root_override=args.root, resume=args.resume, window=args.window,
                         writer_workers=args.writer_workers, backup_every=args.backup_every,
                         period_floor=args.period_floor, required_seconds=args.required_seconds))
