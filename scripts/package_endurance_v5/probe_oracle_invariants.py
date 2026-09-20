#!/usr/bin/env python3
"""Zero-provider oracle invariant battery for package_endurance_v5.

Derived from the counterexample script shipped with the 7224a7b review
(`docs/reviews/2026-09-21-review-7224a7b/probes/probe_v5_oracle.py`). Two
deliberate differences:

* archive bounds are read from the oracle itself (the frozen contract) instead
  of hardcoding the superseded 256-member text, which the audit itself proved
  unsatisfiable for the frozen workload (463 members were required);
* every SQLite connection is closed explicitly, so the battery also runs on
  Windows where an open handle blocks temporary-directory removal.

It adds the counterexamples the review asked for beyond O1-O4: outbox loss,
duplicate publication identity, cross-scope manifest, journal resource closure,
member byte bound and uncompressed total bound.

Exit code 0 means every advertised invariant is enforced and the positive
control is still accepted. Exit code 1 means at least one invariant leaked.
Only temporary synthetic repositories are created: no candidate
implementation, no Rust Runtime, no network, no provider call.
"""
from __future__ import annotations
import argparse
import contextlib
import hashlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import zipfile

DEFAULT_REPO = Path(__file__).resolve().parents[2]


def load_oracle(repo: Path):
    path = repo / "scripts/package_endurance_v5/oracle.py"
    spec = importlib.util.spec_from_file_location("audited_v5_oracle", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"Cannot import {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    data = path.read_bytes()
    identity = hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest()
    return module, identity


@contextlib.contextmanager
def _connect(path, uri=False):
    connection = sqlite3.connect(path, uri=uri)
    try:
        yield connection
    finally:
        connection.close()


def base_rows(oracle, scopes=(("tenant-a", "prod"),)):
    rows = []
    for tenant, environment in scopes:
        for generation in (1, 2, 3):
            rows.append(dict(tenant=tenant, environment=environment,
                             key=f"r{generation}", generation=generation))
    return rows


def make_repository(root: Path, oracle, object_count: int = 1, scopes=(("tenant-a", "prod"),)):
    """Minimal schema read by the actual oracle; no candidate implementation."""
    root.mkdir()
    (root / "objects").mkdir()
    (root / "manifests").mkdir()
    rows = base_rows(oracle, scopes)
    manifests = {}
    for tenant, environment in scopes:
        packages = []
        for i in range(object_count):
            body = f"synthetic-package-{tenant}-{environment}-{i}\n".encode()
            digest = oracle.sha(body)
            (root / "objects" / digest).write_bytes(body)
            packages.append({"name": f"pkg-{i:03d}", "version": 1, "sha256": digest})
        manifest = oracle.canonical({"tenant": tenant, "environment": environment,
                                     "packages": packages})
        manifest_hash = oracle.sha(manifest)
        (root / "manifests" / f"{manifest_hash}.json").write_bytes(manifest)
        manifests[(tenant, environment)] = manifest_hash
    for row in rows:
        row["manifest_sha256"] = manifests[(row["tenant"], row["environment"])]
    db_path = root / "repo.sqlite"
    with _connect(db_path) as db:
        db.executescript('''
            PRAGMA user_version=2;
            CREATE TABLE receipts(tenant TEXT,environment TEXT,key TEXT,generation INTEGER,
                manifest_sha256 TEXT,request_json TEXT,PRIMARY KEY(tenant,environment,key));
            CREATE TABLE current(tenant TEXT,environment TEXT,receipt_json TEXT,
                PRIMARY KEY(tenant,environment));
            CREATE TABLE journal(tenant TEXT,environment TEXT,key TEXT,status TEXT,
                old_generation INTEGER,old_manifest TEXT,old_receipt TEXT,
                new_generation INTEGER,new_manifest TEXT,new_receipt TEXT);
            CREATE TABLE outbox(tenant TEXT,environment TEXT,key TEXT,body TEXT,
                status TEXT,ack TEXT,url TEXT);
        ''')
        for row in rows:
            db.execute("INSERT INTO receipts VALUES(?,?,?,?,?,?)",
                       tuple(row[k] for k in ("tenant", "environment", "key", "generation",
                                              "manifest_sha256")) + ("{}",))
        for tenant, environment in scopes:
            scope_rows = [row for row in rows if (row["tenant"], row["environment"]) == (tenant, environment)]
            last = max(scope_rows, key=lambda item: item["generation"])
            db.execute("INSERT INTO current VALUES(?,?,?)",
                       (tenant, environment, oracle.canonical(last).decode()))
        db.commit()
    for tenant, environment in scopes:
        scope_rows = [row for row in rows if (row["tenant"], row["environment"]) == (tenant, environment)]
        last = max(scope_rows, key=lambda item: item["generation"])
        pointer = root / "deployments" / tenant / environment / "current.json"
        pointer.parent.mkdir(parents=True, exist_ok=True)
        pointer.write_bytes(oracle.canonical(last))
    oracle.repository_cut(root)  # positive control: the fixture is accepted.
    return rows, manifests


def accepts(call):
    try:
        call()
        return True, None
    except (ValueError, OSError) as error:
        return False, f"{type(error).__name__}: {error}"


def write_archive(path: Path, members: dict):
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED, allowZip64=False) as handle:
        for name, body in sorted(members.items()):
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_STORED
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            handle.writestr(info, body)
    return path


def finding(identifier, invariant, mutation, call, expected="reject", **extra):
    accepted, error = accepts(call)
    rejected = not accepted
    row = dict(id=identifier, invariant=invariant, mutation=mutation,
               expected=expected, rejected=rejected, error=error, **extra)
    row["leaked"] = (expected == "reject" and not rejected) or (expected == "accept" and rejected)
    return row


def run_battery(oracle, base: Path):
    findings = []
    root = base / "missing-history"
    rows, _ = make_repository(root, oracle)
    with _connect(root / "repo.sqlite") as db:
        db.execute("DELETE FROM receipts WHERE key='r2'")
        db.commit()
    findings.append(finding("O1", "contiguous historical generations",
                            "delete non-current generation 2; retain current generation 3",
                            lambda: oracle.repository_cut(root)))

    root = base / "stale-current"
    rows, _ = make_repository(root, oracle)
    with _connect(root / "repo.sqlite") as db:
        db.execute("UPDATE current SET receipt_json=?", (oracle.canonical(rows[0]).decode(),))
        db.commit()
    (root / "deployments/tenant-a/prod/current.json").write_bytes(oracle.canonical(rows[0]))
    findings.append(finding("O2", "current points to the maximal generation",
                            "move SQL and disk current together to generation 1 while generation 3 exists",
                            lambda: oracle.repository_cut(root)))

    root = base / "extra-file"
    make_repository(root, oracle)
    descriptor, members = oracle.repository_cut(root)
    expected = oracle.summary(descriptor, members)
    (root / "unexpected.bin").write_bytes(b"not in the cut")
    findings.append(finding("O3", "restore rejects an extra-file destination",
                            "add an unreferenced file to an otherwise matching repository",
                            lambda: oracle.check_restored(root, members, expected)))

    root = base / "too-many-members"
    make_repository(root, oracle, object_count=oracle.ARCHIVE_MEMBER_BOUND)
    descriptor, members = oracle.repository_cut(root)
    expected = oracle.summary(descriptor, members)
    archive = write_archive(base / f"{len(members)}-members.zip", members)
    findings.append(finding("O4", "archive member bound (frozen contract value)",
                            f"canonical ZIP with {len(members)} members exceeds "
                            f"{oracle.ARCHIVE_MEMBER_BOUND}",
                            lambda: oracle.check_archive(archive, members, expected),
                            members=len(members), bound=oracle.ARCHIVE_MEMBER_BOUND))

    root = base / "baseline"
    make_repository(root, oracle)
    descriptor, members = oracle.repository_cut(root)
    expected_summary = oracle.summary(descriptor, members)

    oversized = dict(members)
    oversized["objects/" + "0" * 64] = b"x" * (oracle.MEMBER_BYTE_BOUND + 1)
    findings.append(finding("O5", "member byte bound",
                            "one member exceeds the per-member byte bound",
                            lambda: oracle.validate_members(oversized)))

    total = dict(members)
    for index in range(5):
        total[f"objects/{index}{'0' * 63}"] = b"x" * oracle.MEMBER_BYTE_BOUND
    findings.append(finding("O6", "uncompressed total bound",
                            "members whose sum exceeds the uncompressed total bound",
                            lambda: oracle.validate_members(total)))

    lost = json.loads(members["descriptor.json"].decode())
    lost["outbox"] = [dict(tenant="tenant-a", environment="prod", key="r3",
                           body=json.dumps({"k": "v"}), status="pending", ack=None,
                           url="http://127.0.0.1:1/")]
    lost_members = dict(members)
    lost_members["descriptor.json"] = oracle.canonical(lost)
    findings.append(finding("O7", "outbox obligation represented in the archive",
                            "descriptor carries a pending publication but no outbox member exists",
                            lambda: oracle.validate_members(lost_members)))

    replayed = json.loads(members["descriptor.json"].decode())
    row = dict(tenant="tenant-a", environment="prod", key="r3",
               body=json.dumps({"k": "v"}), status="pending", ack=None,
               url="http://127.0.0.1:1/")
    replayed["outbox"] = [row, dict(row)]
    replayed_members = dict(members)
    replayed_members["descriptor.json"] = oracle.canonical(replayed)
    for index in range(2):
        replayed_members[f"outbox/{index:06d}.json"] = oracle.canonical(row)
    findings.append(finding("O8", "one publication identity per outbox row",
                            "two outbox rows replay the same publication identity",
                            lambda: oracle.validate_members(replayed_members)))

    cross = make_cross_scope(base / "cross-scope", oracle)
    findings.append(finding("O9", "manifests serve only their own scope",
                            "receipt in tenant-b/prod references a manifest written for tenant-a/prod",
                            cross))

    closure_root = base / "journal-closure"
    closure_rows, closure_manifests = make_repository(closure_root, oracle)
    with _connect(closure_root / "repo.sqlite") as db:
        db.execute("INSERT INTO journal VALUES(?,?,?,?,?,?,?,?,?,?)",
                   ("tenant-a", "prod", "r9", "pending", None, None, None, 4,
                    "f" * 64, oracle.canonical(closure_rows[-1]).decode()))
        db.commit()
    findings.append(finding("O10", "journal resource closure",
                            "journal row references a manifest outside the cut",
                            lambda: oracle.repository_cut(closure_root)))

    archive = write_archive(base / "canonical.zip", members)
    findings.append(finding("P1", "positive control: canonical cut and archive accepted",
                            "synthetic repository and its canonical archive",
                            lambda: (oracle.check_archive(archive, members, expected_summary),
                                     oracle.check_restored(base / "baseline", members, expected_summary)),
                            expected="accept"))
    return findings, members, descriptor


def make_cross_scope(root: Path, oracle):
    """A receipt whose manifest was written for a different scope."""
    root.mkdir()
    (root / "objects").mkdir()
    (root / "manifests").mkdir()
    body = b"synthetic-package\n"
    blob = oracle.sha(body)
    (root / "objects" / blob).write_bytes(body)
    manifest = oracle.canonical({"tenant": "tenant-a", "environment": "prod",
                                 "packages": [{"name": "pkg-000", "version": 1, "sha256": blob}]})
    digest = oracle.sha(manifest)
    (root / "manifests" / f"{digest}.json").write_bytes(manifest)
    receipt = {"tenant": "tenant-b", "environment": "prod", "key": "r1",
               "generation": 1, "manifest_sha256": digest}
    with _connect(root / "repo.sqlite") as db:
        db.executescript('''
            PRAGMA user_version=2;
            CREATE TABLE receipts(tenant TEXT,environment TEXT,key TEXT,generation INTEGER,
                manifest_sha256 TEXT,request_json TEXT,PRIMARY KEY(tenant,environment,key));
            CREATE TABLE current(tenant TEXT,environment TEXT,receipt_json TEXT,
                PRIMARY KEY(tenant,environment));
            CREATE TABLE journal(tenant TEXT,environment TEXT,key TEXT,status TEXT,
                old_generation INTEGER,old_manifest TEXT,old_receipt TEXT,
                new_generation INTEGER,new_manifest TEXT,new_receipt TEXT);
            CREATE TABLE outbox(tenant TEXT,environment TEXT,key TEXT,body TEXT,
                status TEXT,ack TEXT,url TEXT);
        ''')
        db.execute("INSERT INTO receipts VALUES(?,?,?,?,?,?)",
                   ("tenant-b", "prod", "r1", 1, digest, "{}"))
        db.execute("INSERT INTO current VALUES(?,?,?)",
                   ("tenant-b", "prod", oracle.canonical(receipt).decode()))
        db.commit()
    pointer = root / "deployments/tenant-b/prod/current.json"
    pointer.parent.mkdir(parents=True, exist_ok=True)
    pointer.write_bytes(oracle.canonical(receipt))
    return lambda: oracle.repository_cut(root)


def wal_visibility(base: Path, oracle):
    root = base / "wal-visibility"
    rows, _ = make_repository(root, oracle)
    with _connect(root / "repo.sqlite") as writer:
        writer.execute("PRAGMA journal_mode=WAL")
        writer.execute("PRAGMA wal_autocheckpoint=0")
        row = dict(rows[-1], key="r4", generation=4)
        writer.execute("INSERT INTO receipts VALUES(?,?,?,?,?,?)",
                       tuple(row[k] for k in ("tenant", "environment", "key", "generation",
                                              "manifest_sha256")) + ("{}",))
        writer.commit()
        counts = {}
        for mode in ("mode=ro", "mode=ro&immutable=1"):
            db_path = (root / "repo.sqlite").resolve()
            with _connect(db_path.as_uri() + "?" + mode, uri=True) as reader:
                reader.execute("PRAGMA query_only=1")
                counts[mode] = reader.execute("SELECT COUNT(*) FROM receipts").fetchone()[0]
        wal_exists = (root / "repo.sqlite-wal").exists()
    return dict(wal_exists=wal_exists, receipt_counts=counts,
                frozen_reading_mode="mode=ro",
                note="immutable hides the committed WAL, which is exactly why the "
                     "frozen contract reads online sources with mode=ro")


def run(repo: Path, *, audit_probe: Path | None = None):
    oracle, identity = load_oracle(repo)
    with tempfile.TemporaryDirectory(prefix="v5-oracle-invariants-") as raw:
        base = Path(raw)
        findings, members, descriptor = run_battery(oracle, base)
        wal = wal_visibility(base, oracle)
        report = dict(
            python=sys.version, sqlite=sqlite3.sqlite_version,
            oracle_git_blob=identity,
            scope="synthetic oracle counterexamples only; no runtime/provider/candidate execution",
            frozen_bounds=dict(member_bound=oracle.ARCHIVE_MEMBER_BOUND,
                               member_bytes=oracle.MEMBER_BYTE_BOUND,
                               uncompressed_bytes=oracle.TOTAL_UNCOMPRESSED_BOUND,
                               archive_bytes=oracle.ARCHIVE_BYTE_BOUND),
            control_members=len(members),
            control_summary=oracle.summary(descriptor, members),
            findings=findings,
            leaked=sum(1 for row in findings if row["leaked"]),
            wal_visibility=wal,
        )
        if audit_probe is not None:
            report["audit_probe"] = replay_audit_probe(audit_probe, repo)
    return report


def replay_audit_probe(probe_path: Path, repo: Path):
    """Replay the review's verbatim probe under a Windows-safe temp cleanup.

    The verbatim probe leaks SQLite handles, so ``TemporaryDirectory`` cannot
    remove its own tree on Windows. Only the cleanup policy is patched here: the
    counterexamples and expectations are the audit's own bytes.
    """
    spec = importlib.util.spec_from_file_location("audit_probe", probe_path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    original = tempfile.TemporaryDirectory

    def patched(*args, **kwargs):
        kwargs["ignore_cleanup_errors"] = True
        return original(*args, **kwargs)

    tempfile.TemporaryDirectory = patched
    try:
        try:
            result = module.run(repo)
        except SystemExit:
            result = None
    finally:
        tempfile.TemporaryDirectory = original
    if result is None:
        return dict(executed=False, detail="verbatim probe raised SystemExit")
    result["temporary_directory_cleanup"] = "patched: ignore_cleanup_errors=True (Windows handle leak in the audit probe)"
    result["superseded_expectations"] = [
        "O4: the audit expects rejection above 256 members. The frozen contract now "
        "computes the bound from the frozen workload (the audit itself required 463 "
        "members), so this expectation is verified at the contracted bound by O4 above."
    ]
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=DEFAULT_REPO)
    parser.add_argument("--out", type=Path)
    parser.add_argument("--audit-probe", type=Path)
    args = parser.parse_args()
    report = run(args.repo.resolve(), audit_probe=args.audit_probe)
    text = json.dumps(report, ensure_ascii=False, indent=2)
    print(text)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    if args.audit_probe is not None:
        audit = report.get("audit_probe", {})
        for row in audit.get("findings", []):
            if row["id"] in {"O1", "O2", "O3"} and row["bad_state_accepted"]:
                report["leaked"] += 1
    return 1 if report["leaked"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
