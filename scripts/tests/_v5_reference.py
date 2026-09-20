"""In-process reference application and backup/restore for the V5 controller tests.

It stands in for the agent-owned candidate module so the controller, the real
independent oracle and the accepted v3 fixture application can be exercised end
to end with zero provider calls. This is a test fixture, never a production
candidate: nothing here is imported by the controller or by the oracle.
"""
from __future__ import annotations

import hashlib
import importlib
import json
import os
import shutil
import sqlite3
import sys
import threading
import zipfile
from pathlib import Path

TESTS = Path(__file__).resolve().parent
SCRIPTS = TESTS.parent
REPO = SCRIPTS.parent
V5 = SCRIPTS / "package_endurance_v5"

sys.path.insert(0, str(TESTS))

from _v5_modules import import_v5  # noqa: E402

oracle = import_v5("oracle")

ZIP_DATE = (1980, 1, 1, 0, 0, 0)
SCHEMA = """
CREATE TABLE IF NOT EXISTS receipts(
    tenant TEXT NOT NULL, environment TEXT NOT NULL, key TEXT NOT NULL,
    generation INTEGER NOT NULL, manifest_sha256 TEXT NOT NULL,
    request_json TEXT NOT NULL, PRIMARY KEY(tenant, environment, key));
CREATE TABLE IF NOT EXISTS current(
    tenant TEXT NOT NULL, environment TEXT NOT NULL, receipt_json TEXT NOT NULL,
    PRIMARY KEY(tenant, environment));
CREATE TABLE IF NOT EXISTS journal(
    tenant TEXT NOT NULL, environment TEXT NOT NULL, key TEXT NOT NULL,
    status TEXT NOT NULL, old_generation INTEGER, old_manifest TEXT,
    old_receipt TEXT, new_generation INTEGER, new_manifest TEXT,
    new_receipt TEXT NOT NULL,
    PRIMARY KEY(tenant, environment, key));
CREATE TABLE IF NOT EXISTS outbox(
    tenant TEXT NOT NULL, environment TEXT NOT NULL, key TEXT NOT NULL,
    body TEXT NOT NULL, status TEXT NOT NULL, ack TEXT, url TEXT,
    PRIMARY KEY(tenant, environment, key));
"""


def make_workspace(work: Path, packages: int = 4, versions: int = 2) -> dict:
    """Copy the accepted v3 fixture app plus a tiny catalog into a workspace."""
    work = Path(work)
    source = REPO / "scripts/package_endurance_v3/fixture/app"
    for item in source.rglob("*"):
        if not item.is_file() or "__pycache__" in item.parts:
            continue
        target = work / "app" / item.relative_to(source)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(item, target)
    catalog = {}
    for index in range(packages):
        name = f"pkg-{index:03d}"
        catalog[name] = []
        for version in range(1, versions + 1):
            payload = (f"package={name};version={version};\n"
                       + "data-package-line\n" * 4).encode("utf-8")
            blob = hashlib.sha256(payload).hexdigest()
            dependencies = {} if index == 0 else {
                f"pkg-{index - 1:03d}": {"min": version, "max": version + 1}
            }
            catalog[name].append(dict(version=version, sha256=blob, deps=dependencies))
            target = work / "fixtures/blobs" / blob
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(payload)
    (work / "fixtures/catalog.json").write_text(
        json.dumps(catalog, ensure_ascii=False, sort_keys=True, separators=(",", ":")),
        encoding="utf-8")
    (work / "app/live_backup.py").write_text("# reference candidate marker\n", encoding="utf-8")
    (work / "runtime-feedback").mkdir(exist_ok=True)
    return catalog


_APP_LOCK = threading.Lock()
_APP_MODULE = None


def _app_module():
    """Import the accepted fixture application once, from its own directory.

    The application is stateless with respect to the workspace: ``Repository``
    receives the root and the catalog path explicitly, so one import serves
    every workspace and there is no per-call module churn to race on between the
    controller's writer, reader/GC and verify executors.
    """
    global _APP_MODULE
    if _APP_MODULE is None:
        with _APP_LOCK:
            if _APP_MODULE is None:
                fixture = str(REPO / "scripts/package_endurance_v3/fixture")
                if fixture not in sys.path:
                    sys.path.insert(0, fixture)
                _APP_MODULE = importlib.import_module("app.repository")
    return _APP_MODULE


def _repository(work: Path, root):
    return _app_module().Repository(root, Path(work) / "fixtures/catalog.json")


def _archive_bytes(members: dict) -> bytes:
    import io
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_STORED, allowZip64=False) as handle:
        for name, body in sorted(members.items()):
            info = zipfile.ZipInfo(name, date_time=ZIP_DATE)
            info.compress_type = zipfile.ZIP_STORED
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            handle.writestr(info, body)
    return buffer.getvalue()


def backup_live(root, archive, crash_at=None):
    """Reference implementation of the frozen contract's backup entry point."""
    archive = Path(archive)
    descriptor, members = oracle.repository_cut(root)
    oracle.validate_members(members)
    payload = _archive_bytes(members)
    temporary = archive.with_name(archive.name + ".part")
    temporary.write_bytes(payload)
    if crash_at == "before_publish":
        raise CrashBoundary("before_publish")
    if archive.exists():
        raise ValueError("destination archive already exists")
    os.replace(temporary, archive)
    return oracle.summary(descriptor, members)


def restore_live(archive, destination, crash_at=None):
    """Reference implementation of the frozen contract's restore entry point."""
    archive, destination = Path(archive), Path(destination)
    with zipfile.ZipFile(archive) as handle:
        members = {info.filename: handle.read(info) for info in handle.infolist()}
    descriptor = oracle.validate_members(members)
    staging = destination.with_name(destination.name + ".stage")
    if staging.exists() or destination.exists():
        raise ValueError("destination or stage already exists")
    staging.mkdir(parents=True)
    try:
        for name, body in members.items():
            if name == "descriptor.json":
                continue
            path = staging / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(body)
        _write_authority(staging, descriptor)
        if crash_at == "before_publish":
            raise CrashBoundary("before_publish")
        os.rename(staging, destination)
    except BaseException:
        if crash_at != "before_publish":
            shutil.rmtree(staging, ignore_errors=True)
        raise
    return oracle.summary(descriptor, members)


class CrashBoundary(RuntimeError):
    """The child-process boundary the frozen contract requires (exit code 74)."""


def _write_authority(destination: Path, descriptor: dict) -> None:
    database = Path(destination) / "repo.sqlite"
    connection = sqlite3.connect(database, isolation_level=None)
    try:
        connection.executescript(SCHEMA)
        connection.execute("BEGIN")
        for row in descriptor["receipts"]:
            connection.execute(
                "INSERT OR REPLACE INTO receipts VALUES(?,?,?,?,?,?)",
                (row["tenant"], row["environment"], row["key"], row["generation"],
                 row["manifest_sha256"], row["request_json"]))
        for row in descriptor["current"]:
            connection.execute("INSERT OR REPLACE INTO current VALUES(?,?,?)",
                               (row["tenant"], row["environment"],
                                oracle.canonical(row).decode("utf-8")))
        for row in descriptor["journal"]:
            connection.execute(
                "INSERT OR REPLACE INTO journal VALUES(?,?,?,?,?,?,?,?,?,?)",
                tuple(row[field] for field in oracle.JOURNAL_FIELDS))
        for row in descriptor["outbox"]:
            connection.execute(
                "INSERT OR REPLACE INTO outbox VALUES(?,?,?,?,?,?,?)",
                tuple(row[field] for field in oracle.OUTBOX_FIELDS))
        connection.execute("PRAGMA user_version=2")
        connection.execute("COMMIT")
    finally:
        connection.close()


def dispatcher(*, honor_crash=True, failing=None):
    """Build a ``candidate_process``-shaped callable.

    ``failing`` maps an operation to a predicate, so a test can script one
    concrete unresolved failure without touching the controller.
    """
    state = dict(calls=[], crash_calls=0)
    failing = failing or {}

    def candidate_process(work, operation, kwargs, *, root=None, timeout=45):
        state["calls"].append(operation)
        if failing.get(operation, lambda *_: False)(kwargs):
            return dict(returncode=1, value=dict(ok=False, error_type="ValueError",
                                                 error=f"scripted {operation} failure"),
                        stderr="")
        try:
            if operation in ("backup_live", "restore_live"):
                crash_at = kwargs.get("crash_at")
                try:
                    if operation == "backup_live":
                        value = backup_live(kwargs["root"], kwargs["archive"], crash_at=crash_at)
                    else:
                        value = restore_live(kwargs["archive"], kwargs["destination"], crash_at=crash_at)
                except CrashBoundary:
                    state["crash_calls"] += 1
                    if not honor_crash:
                        return dict(returncode=0,
                                    value=dict(ok=True, value={"note": "boundary ignored"}),
                                    stderr="")
                    return dict(returncode=74, value=dict(ok=False, error_type="CrashBoundary",
                                                          error="before_publish"), stderr="")
            else:
                repository = _repository(work, root)
                with repository:
                    value = getattr(repository, operation)(**kwargs)
            return dict(returncode=0, value=dict(ok=True, value=value), stderr="")
        except Exception as error:  # candidate failures are evidence, not crashes
            return dict(returncode=1, value=dict(ok=False, error_type=type(error).__name__,
                                                 error=str(error)), stderr="")

    candidate_process.state = state
    return candidate_process
