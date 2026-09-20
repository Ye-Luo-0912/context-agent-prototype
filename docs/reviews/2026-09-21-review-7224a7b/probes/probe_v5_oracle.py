#!/usr/bin/env python3
"""Zero-provider counterexamples for package_endurance_v5/oracle.py.

Only temporary synthetic SQLite repositories are created. This is not a
Runtime/candidate end-to-end test. Run against a repository checkout with:
  python probe_v5_oracle.py --repo /path/to/context-agent-prototype --out results.json

Exit 1 means at least one advertised oracle invariant was not enforced.
The checked oracle's actual Git blob SHA is included in the result.
"""
from __future__ import annotations
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import zipfile


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


def make_repository(root: Path, oracle, object_count: int = 1):
    """Minimal schema read by the actual oracle; no candidate implementation."""
    root.mkdir()
    (root / "objects").mkdir()
    (root / "manifests").mkdir()
    pointer = root / "deployments/tenant-a/prod/current.json"
    pointer.parent.mkdir(parents=True)
    packages = []
    for i in range(object_count):
        body = f"synthetic-package-{i}\n".encode()
        digest = oracle.sha(body)
        (root / "objects" / digest).write_bytes(body)
        packages.append({"name": f"pkg-{i:03d}", "version": 1, "sha256": digest})
    manifest = oracle.canonical({"tenant": "tenant-a", "environment": "prod", "packages": packages})
    manifest_hash = oracle.sha(manifest)
    (root / "manifests" / f"{manifest_hash}.json").write_bytes(manifest)
    rows = [{"tenant": "tenant-a", "environment": "prod", "key": f"r{g}",
             "generation": g, "manifest_sha256": manifest_hash} for g in (1, 2, 3)]
    db = sqlite3.connect(root / "repo.sqlite")
    try:
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
                       tuple(row[k] for k in ("tenant", "environment", "key", "generation", "manifest_sha256")) + ("{}",))
        db.execute("INSERT INTO current VALUES(?,?,?)", ("tenant-a", "prod", oracle.canonical(rows[-1]).decode()))
        db.commit()
    finally:
        db.close()
    pointer.write_bytes(oracle.canonical(rows[-1]))
    oracle.repository_cut(root)  # positive control: the fixture is accepted.
    return rows, pointer


def accepts(call):
    try:
        call()
        return True, None
    except (ValueError, OSError) as error:
        return False, f"{type(error).__name__}: {error}"


def run(repo: Path):
    oracle, identity = load_oracle(repo)
    findings = []
    with tempfile.TemporaryDirectory(prefix="v5-oracle-audit-") as raw:
        base = Path(raw)
        root = base / "missing-history"
        rows, pointer = make_repository(root, oracle)
        with sqlite3.connect(root / "repo.sqlite") as db:
            db.execute("DELETE FROM receipts WHERE key='r2'")
        accepted, error = accepts(lambda: oracle.repository_cut(root))
        findings.append(dict(id="O1", invariant="contiguous historical generations",
                             mutation="delete non-current generation 2; retain current generation 3",
                             bad_state_accepted=accepted, error=error))

        root = base / "stale-current"
        rows, pointer = make_repository(root, oracle)
        with sqlite3.connect(root / "repo.sqlite") as db:
            db.execute("UPDATE current SET receipt_json=?", (oracle.canonical(rows[0]).decode(),))
        pointer.write_bytes(oracle.canonical(rows[0]))
        accepted, error = accepts(lambda: oracle.repository_cut(root))
        findings.append(dict(id="O2", invariant="current points to maximal generation",
                             mutation="move SQL and disk current together to generation 1 while generation 3 exists",
                             bad_state_accepted=accepted, error=error))

        root = base / "extra-file"
        make_repository(root, oracle)
        descriptor, members = oracle.repository_cut(root)
        expected = oracle.summary(descriptor, members)
        (root / "unexpected.bin").write_bytes(b"not in the cut")
        accepted, error = accepts(lambda: oracle.check_restored(root, members, expected))
        findings.append(dict(id="O3", invariant="restore rejects extra-file destination",
                             mutation="add an unreferenced file to an otherwise matching repository",
                             bad_state_accepted=accepted, error=error))

        root = base / "too-many-members"
        make_repository(root, oracle, object_count=260)
        descriptor, members = oracle.repository_cut(root)
        expected = oracle.summary(descriptor, members)
        archive = base / "263-members.zip"
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_STORED, allowZip64=False) as z:
            for name, body in sorted(members.items()):
                info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_STORED
                info.create_system = 3
                info.external_attr = 0o100644 << 16
                z.writestr(info, body)
        accepted, error = accepts(lambda: oracle.check_archive(archive, members, expected))
        findings.append(dict(id="O4", invariant="at most 256 archive members",
                             members=len(members), bytes=archive.stat().st_size,
                             bad_state_accepted=accepted, error=error))

        # A deterministic SQLite demonstration, not a run of the missing candidate.
        root = base / "wal-visibility"
        rows, pointer = make_repository(root, oracle)
        db = sqlite3.connect(root / "repo.sqlite")
        try:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA wal_autocheckpoint=0")
            row = dict(rows[-1], key="r4", generation=4)
            db.execute("INSERT INTO receipts VALUES(?,?,?,?,?,?)",
                       tuple(row[k] for k in ("tenant", "environment", "key", "generation", "manifest_sha256")) + ("{}",))
            db.commit()
            counts = {}
            for mode in ("mode=ro", "mode=ro&immutable=1"):
                reader = sqlite3.connect((root / "repo.sqlite").as_uri() + "?" + mode, uri=True)
                try:
                    counts[mode] = reader.execute("SELECT COUNT(*) FROM receipts").fetchone()[0]
                finally:
                    reader.close()
            wal_demo = dict(wal_exists=(root / "repo.sqlite-wal").exists(), receipt_counts=counts)
        finally:
            db.close()
    return dict(source_git_blob=identity,
                expected_review_blob="b459e27152c6b2ea8cb8416ce25cc5002eebe3ab",
                python=sys.version, sqlite=sqlite3.sqlite_version,
                scope="synthetic oracle counterexamples only; no runtime/provider/candidate execution",
                findings=findings, wal_visibility=wal_demo,
                oracle_invariant_failures=sum(row["bad_state_accepted"] for row in findings))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, required=True)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    result = run(args.repo.resolve())
    text = json.dumps(result, ensure_ascii=False, indent=2)
    print(text)
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(text + "\n", encoding="utf-8")
    return 1 if result["oracle_invariant_failures"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
