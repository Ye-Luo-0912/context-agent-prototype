"""Independent V5 oracle. It never imports candidate backup code."""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import struct
import subprocess
import sys
import tempfile
import zipfile

NAME_FIELDS = ("tenant", "environment", "key")
RECEIPT_FIELDS = ("tenant", "environment", "key", "generation", "manifest_sha256")


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True,
                      separators=(",", ":"), allow_nan=False).encode("utf-8")


def sha(value):
    return hashlib.sha256(value).hexdigest()


def read_json(path):
    data = Path(path).read_bytes()
    value = json.loads(data.decode("utf-8"))
    if canonical(value) != data:
        raise ValueError(f"noncanonical JSON: {path}")
    return value


def bytes_map(root):
    root = Path(root)
    if not root.exists():
        return {}
    result = {}
    for base, dirs, names in os.walk(root, followlinks=False):
        for name in sorted(dirs + names):
            path = Path(base) / name
            info = path.lstat()
            rel = path.relative_to(root).as_posix()
            if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
                result[rel] = ("link", os.readlink(path))
                if name in dirs:
                    dirs.remove(name)
            elif path.is_dir():
                result[rel] = ("directory",)
            else:
                result[rel] = ("file", sha(path.read_bytes()))
    return result


def _safe_regular(path):
    info = Path(path).lstat()
    if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
        raise ValueError(f"link/reparse point: {path}")
    if not stat.S_ISREG(info.st_mode):
        raise ValueError(f"not a regular file: {path}")


def _connection(root):
    db = (Path(root) / "repo.sqlite").resolve()
    _safe_regular(db)
    # A live repository may have a committed WAL. `mode=ro` lets SQLite include
    # that authority state without allowing writes; immutable would incorrectly
    # hide the WAL and make a valid online cut look like an empty schema.
    uri = db.as_uri() + "?mode=ro"
    return sqlite3.connect(uri, uri=True)


def repository_cut(root):
    """Read one authority point and referenced bytes from real disk."""
    root = Path(root).resolve()
    db = _connection(root)
    try:
        if db.execute("PRAGMA user_version").fetchone()[0] != 2:
            raise ValueError("unsupported schema")
        tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if not {"receipts", "current", "journal", "outbox"} <= tables:
            raise ValueError("missing authority table")
        receipts = [dict(zip(("tenant", "environment", "key", "generation",
                              "manifest_sha256", "request_json"), row)) for row in db.execute(
            "SELECT tenant,environment,key,generation,manifest_sha256,request_json "
            "FROM receipts ORDER BY tenant,environment,key")]
        current = []
        for tenant, environment, text in db.execute(
                "SELECT tenant,environment,receipt_json FROM current ORDER BY tenant,environment"):
            row = json.loads(text)
            if canonical(row).decode("utf-8") != text:
                raise ValueError("noncanonical current SQL row")
            if (row.get("tenant"), row.get("environment")) != (tenant, environment):
                raise ValueError("current scope mismatch")
            current.append(row)
        journal = [dict(zip(("tenant", "environment", "key", "status", "old_generation",
                             "old_manifest", "old_receipt", "new_generation", "new_manifest",
                             "new_receipt"), row)) for row in db.execute(
            "SELECT tenant,environment,key,status,old_generation,old_manifest,old_receipt,"
            "new_generation,new_manifest,new_receipt FROM journal ORDER BY tenant,environment,key")]
        outbox = [dict(zip(("tenant", "environment", "key", "body", "status", "ack", "url"), row))
                  for row in db.execute(
            "SELECT tenant,environment,key,body,status,ack,url FROM outbox ORDER BY tenant,environment,key")]
    finally:
        db.close()
    descriptor = dict(format="online-package-backup-v1", schema_version=2,
                      receipts=receipts, current=current, journal=journal, outbox=outbox)
    receipt_map = {(row["tenant"], row["environment"], row["key"]): row for row in receipts}
    for row in current:
        identity = (row.get("tenant"), row.get("environment"), row.get("key"))
        if identity not in receipt_map or any(row.get(key) != receipt_map[identity].get(key)
                                             for key in RECEIPT_FIELDS):
            raise ValueError("current row has no matching historical receipt")
    manifests = {}
    objects = {}
    for receipt in receipts:
        digest = receipt["manifest_sha256"]
        path = root / "manifests" / (digest + ".json")
        _safe_regular(path)
        data = path.read_bytes()
        if sha(data) != digest:
            raise ValueError("manifest hash mismatch")
        manifest = json.loads(data.decode("utf-8"))
        if canonical(manifest) != data:
            raise ValueError("manifest is not canonical")
        manifests[digest] = data
        for package in manifest.get("packages", []):
            blob = package["sha256"]
            blob_path = root / "objects" / blob
            _safe_regular(blob_path)
            blob_data = blob_path.read_bytes()
            if sha(blob_data) != blob:
                raise ValueError("object hash mismatch")
            objects[blob] = blob_data
    pointers = {}
    for row in current:
        relative = Path("deployments") / row["tenant"] / row["environment"] / "current.json"
        path = root / relative
        _safe_regular(path)
        if path.read_bytes() != canonical(row):
            raise ValueError("current pointer mismatch")
        pointers[relative.as_posix()] = path.read_bytes()
    members = {"descriptor.json": canonical(descriptor)}
    members.update({f"manifests/{key}.json": value for key, value in manifests.items()})
    members.update({f"objects/{key}": value for key, value in objects.items()})
    for index, row in enumerate(journal):
        members[f"journal/{index:06d}.json"] = canonical(row)
    for index, row in enumerate(outbox):
        members[f"outbox/{index:06d}.json"] = canonical(row)
    members.update(pointers)
    return descriptor, members


def summary(descriptor, members):
    return dict(cut_id=sha(members["descriptor.json"]),
                scopes=len(descriptor["current"]), receipts=len(descriptor["receipts"]),
                manifests=sum(name.startswith("manifests/") for name in members),
                objects=sum(name.startswith("objects/") for name in members),
                outbox=len(descriptor["outbox"]))


def check_archive(archive, expected_members, expected_summary):
    archive = Path(archive)
    data = archive.read_bytes()
    if len(data) > 9 * 1024 * 1024 or len(data) < 22:
        raise ValueError("archive size bound")
    with zipfile.ZipFile(archive) as z:
        infos = z.infolist()
        names = [info.filename for info in infos]
        if names != sorted(set(names)) or names != sorted(expected_members):
            raise ValueError("archive members/order mismatch")
        for info in infos:
            if info.compress_type != zipfile.ZIP_STORED or info.date_time != (1980, 1, 1, 0, 0, 0):
                raise ValueError("noncanonical ZIP metadata")
            if info.create_system != 3 or info.external_attr != (0o100644 << 16):
                raise ValueError("unsafe ZIP metadata")
            if info.file_size > 2 * 1024 * 1024 or info.extra or info.comment or info.flag_bits & 1:
                raise ValueError("ZIP bound or link violation")
            if z.read(info) != expected_members[info.filename]:
                raise ValueError("archive content mismatch")
    descriptor = json.loads(expected_members["descriptor.json"])
    if summary(descriptor, expected_members) != expected_summary:
        raise ValueError("summary mismatch")


def check_restored(destination, expected_members, expected_summary):
    destination = Path(destination)
    descriptor, members = repository_cut(destination)
    if summary(descriptor, members) != expected_summary:
        raise ValueError("restored summary mismatch")
    for name, data in expected_members.items():
        if name == "descriptor.json":
            continue
        if name.startswith("deployments/") or name.startswith("journal/") or name.startswith("outbox/"):
            continue
        path = destination / name
        if path.read_bytes() != data:
            raise ValueError(f"restored member mismatch: {name}")


def invoke(work, operation, **kwargs):
    root = kwargs.pop("root", None)
    result = subprocess.run([sys.executable, str(Path(__file__).with_name("invoke.py")), str(work)],
                            input=json.dumps({"op": operation, "root": str(root) if root is not None else None,
                                              "kwargs": kwargs}),
                            capture_output=True, text=True, timeout=45)
    if result.returncode:
        raise RuntimeError(result.stderr[-2000:])
    return json.loads(result.stdout)


def calibration(work):
    """Create a real repository and prove the oracle rejects three mutations."""
    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw) / "repository"
        request = dict(tenant="tenant-a", environment="prod", key="seed",
                       requirements={"pkg-000": {"min": 1, "max": 2}})
        result = invoke(work, "install", root=str(root), **request)
        if not result.get("ok"):
            raise AssertionError(result)
        descriptor, members = repository_cut(root)
        expected = summary(descriptor, members)
        rejected = []
        blob = next((root / "objects").iterdir())
        original = blob.read_bytes()
        blob.write_bytes(b"corrupt")
        try:
            repository_cut(root)
        except (ValueError, OSError):
            rejected.append("object")
        blob.write_bytes(original)
        pointer = root / "deployments/tenant-a/prod/current.json"
        original = pointer.read_bytes()
        pointer.write_bytes(b"{}")
        try:
            repository_cut(root)
        except (ValueError, OSError):
            rejected.append("pointer")
        pointer.write_bytes(original)
        db = sqlite3.connect(root / "repo.sqlite")
        try:
            db.execute("DELETE FROM receipts")
            db.commit()
        finally:
            db.close()
        try:
            repository_cut(root)
        except (ValueError, OSError):
            rejected.append("receipt")
        if rejected != ["object", "pointer", "receipt"]:
            raise AssertionError((rejected, expected))
        return dict(summary=expected, rejected_mutations=rejected)
