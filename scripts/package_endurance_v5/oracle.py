"""Independent V5 oracle. It never imports candidate backup code.

Every bound enforced here is declared once, in this module, so the contract
text (``SPEC.md``), the capacity plan (``prepare.py``) and the readiness check
(``preflight.py``) cannot drift apart from what is actually enforced.

The cut identity is established inside one explicit read transaction; the
controller is responsible for keeping the source quiescent for the duration of
the call (see the frozen contract), and records the source fingerprint before
and after the call as the cut-interval evidence.
"""
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat
import subprocess
import sys
import zipfile

NAME_FIELDS = ("tenant", "environment", "key")
RECEIPT_FIELDS = ("tenant", "environment", "key", "generation", "manifest_sha256")
RECEIPT_ROW_FIELDS = RECEIPT_FIELDS + ("request_json",)
JOURNAL_FIELDS = ("tenant", "environment", "key", "status", "old_generation", "old_manifest",
                  "old_receipt", "new_generation", "new_manifest", "new_receipt")
OUTBOX_FIELDS = ("tenant", "environment", "key", "body", "status", "ack", "url")
DESCRIPTOR_FIELDS = ("format", "schema_version", "receipts", "current", "journal", "outbox")
MEMBER_PREFIXES = ("manifests/", "objects/", "deployments/", "journal/", "outbox/")

# Frozen archive bounds. SPEC.md must state exactly these numbers; the
# preflight check compares the contract text against these values.
ARCHIVE_MEMBER_BOUND = 1024
MEMBER_BYTE_BOUND = 2 * 1024 * 1024
TOTAL_UNCOMPRESSED_BOUND = 8 * 1024 * 1024
ARCHIVE_BYTE_BOUND = 9 * 1024 * 1024

# SQLite sidecars of the restored authority. A restored repository is opened by
# the application in WAL mode, so these are expected; nothing else may be added.
RESTORED_SIDECARS = ("repo.sqlite-wal", "repo.sqlite-shm")
RESTORED_DATABASE = "repo.sqlite"


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


def source_fingerprint(root):
    """Content fingerprint of the source, used as cut-interval evidence.

    The controller takes one before and one after a candidate call. Equal
    fingerprints prove the interval was quiescent; a difference means the cut
    cannot be attributed and the batch must be refused rather than counted as a
    candidate failure.

    The SQLite shared-memory sidecar and an *empty* WAL are excluded: a
    read-only reader may legitimately attach to (or open) them without changing
    any authority. A non-empty WAL is part of the authority and is hashed.
    """
    root = Path(root).resolve()
    if not (root / RESTORED_DATABASE).exists():
        return dict(files=0, fingerprint=None)
    entries = {}
    for name, entry in bytes_map(root).items():
        if name == RESTORED_DATABASE + "-shm":
            continue
        if name == RESTORED_DATABASE + "-wal":
            path = root / name
            if path.exists() and path.stat().st_size == 0:
                continue
        entries[name] = entry
    return dict(files=len(entries), fingerprint=sha(canonical(entries)))


def _safe_regular(path):
    info = Path(path).lstat()
    if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
        raise ValueError(f"link/reparse point: {path}")
    if not stat.S_ISREG(info.st_mode):
        raise ValueError(f"not a regular file: {path}")


def _connection(root):
    """One percent-encoded read-only connection.

    ``mode=ro`` is the frozen reading mode: it includes any committed WAL that
    is part of the authority and still refuses every write. ``immutable=1`` is
    deliberately not used: SQLite defines it as a promise that the file cannot
    change, which is false for an online source and would silently hide a
    committed WAL.
    """
    db = (Path(root) / RESTORED_DATABASE).resolve()
    _safe_regular(db)
    uri = db.as_uri() + "?mode=ro"
    connection = sqlite3.connect(uri, uri=True, isolation_level=None)
    connection.execute("PRAGMA query_only=1")
    return connection


def _read_authority(db):
    """Read every authority row inside one explicit read transaction."""
    db.execute("BEGIN")
    try:
        if db.execute("PRAGMA user_version").fetchone()[0] != 2:
            raise ValueError("unsupported schema")
        tables = {row[0] for row in db.execute(
            "SELECT name FROM sqlite_master WHERE type='table'")}
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
        journal = [dict(zip(JOURNAL_FIELDS, row)) for row in db.execute(
            "SELECT tenant,environment,key,status,old_generation,old_manifest,old_receipt,"
            "new_generation,new_manifest,new_receipt FROM journal ORDER BY tenant,environment,key")]
        outbox = [dict(zip(OUTBOX_FIELDS, row)) for row in db.execute(
            "SELECT tenant,environment,key,body,status,ack,url FROM outbox "
            "ORDER BY tenant,environment,key")]
    finally:
        db.execute("ROLLBACK")
    return receipts, current, journal, outbox


def _check_generations(receipts, current):
    """Every scope keeps a contiguous 1..N history and names N as current."""
    by_scope = {}
    for row in receipts:
        generation = row.get("generation")
        if isinstance(generation, bool) or not isinstance(generation, int) or generation < 1:
            raise ValueError("receipt generation must be a positive integer")
        by_scope.setdefault((row.get("tenant"), row.get("environment")), []).append(generation)
    current_by_scope = {}
    for row in current:
        scope = (row.get("tenant"), row.get("environment"))
        if scope in current_by_scope:
            raise ValueError("duplicate current row for one scope")
        current_by_scope[scope] = row
    for scope, generations in by_scope.items():
        ordered = sorted(generations)
        if ordered != list(range(1, ordered[-1] + 1)):
            raise ValueError(
                f"non-contiguous historical generations in scope {scope}: {ordered}")
        row = current_by_scope.get(scope)
        if row is None:
            raise ValueError(f"scope {scope} has receipts but no current row")
        if row.get("generation") != ordered[-1]:
            raise ValueError(
                f"current generation {row.get('generation')!r} is not the maximal "
                f"generation {ordered[-1]} of scope {scope}")
    for scope in current_by_scope:
        if scope not in by_scope:
            raise ValueError(f"current row for scope {scope} has no historical receipt")


def _check_identity_uniqueness(rows, label):
    seen = set()
    for row in rows:
        identity = (row.get("tenant"), row.get("environment"), row.get("key"))
        if None in identity:
            raise ValueError(f"{label} row misses its identity")
        if identity in seen:
            raise ValueError(f"duplicate {label} identity: {identity}")
        seen.add(identity)


def _check_journal_closure(journal, manifests):
    """A journal row without its resources is an incomplete cut."""
    for row in journal:
        for key in ("old_manifest", "new_manifest"):
            digest = row.get(key)
            if digest and digest not in manifests:
                raise ValueError(f"journal {key} outside the cut: {digest}")
        text = row.get("new_receipt")
        if not text:
            continue
        value = json.loads(text)
        if not isinstance(value, dict):
            raise ValueError("journal receipt is not an object")
        digest = value.get("manifest_sha256")
        if digest and digest not in manifests:
            raise ValueError(f"journal receipt manifest outside the cut: {digest}")


def _check_receipt_manifest_scope(receipts, manifest_objects):
    """A manifest may only serve the scope it was written for."""
    for receipt in receipts:
        digest = receipt.get("manifest_sha256")
        manifest = manifest_objects.get(digest)
        if manifest is None:
            raise ValueError(f"receipt references an absent manifest: {digest}")
        scope = (receipt.get("tenant"), receipt.get("environment"))
        if (manifest.get("tenant"), manifest.get("environment")) != scope:
            raise ValueError(f"cross-scope manifest {digest} for scope {scope}")


def repository_cut(root):
    """Read one authority point and its referenced bytes from real disk.

    The SQL reads happen inside one explicit read transaction, and the
    referenced files are read before that transaction is released. A file that
    changes under a concurrent writer therefore fails hash verification instead
    of silently mixing two cut moments.
    """
    root = Path(root).resolve()
    db = _connection(root)
    try:
        receipts, current, journal, outbox = _read_authority(db)
        _check_generations(receipts, current)
        _check_identity_uniqueness(outbox, "outbox")
        _check_identity_uniqueness(journal, "journal")
        descriptor = dict(format="online-package-backup-v1", schema_version=2,
                          receipts=receipts, current=current, journal=journal, outbox=outbox)
        receipt_map = {(row["tenant"], row["environment"], row["key"]): row for row in receipts}
        for row in current:
            identity = (row.get("tenant"), row.get("environment"), row.get("key"))
            if identity not in receipt_map or any(row.get(key) != receipt_map[identity].get(key)
                                                 for key in RECEIPT_FIELDS):
                raise ValueError("current row has no matching historical receipt")
        manifests = {}
        manifest_objects = {}
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
            manifest_objects[digest] = manifest
            for package in manifest.get("packages", []):
                blob = package["sha256"]
                blob_path = root / "objects" / blob
                _safe_regular(blob_path)
                blob_data = blob_path.read_bytes()
                if sha(blob_data) != blob:
                    raise ValueError("object hash mismatch")
                objects[blob] = blob_data
        _check_receipt_manifest_scope(receipts, manifest_objects)
        _check_journal_closure(journal, manifests)
        pointers = {}
        for row in current:
            relative = Path("deployments") / row["tenant"] / row["environment"] / "current.json"
            path = root / relative
            _safe_regular(path)
            if path.read_bytes() != canonical(row):
                raise ValueError("current pointer mismatch")
            pointers[relative.as_posix()] = path.read_bytes()
    finally:
        db.close()
    members = {"descriptor.json": canonical(descriptor)}
    members.update({f"manifests/{key}.json": value for key, value in manifests.items()})
    members.update({f"objects/{key}": value for key, value in objects.items()})
    for index, row in enumerate(journal):
        members[f"journal/{index:06d}.json"] = canonical(row)
    for index, row in enumerate(outbox):
        members[f"outbox/{index:06d}.json"] = canonical(row)
    members.update(pointers)
    return descriptor, members


def validate_members(members):
    """Structural validation of one member map (archive or reconstructed cut).

    It is deliberately independent of any candidate implementation: it only
    reads the members themselves, so it also rejects a self-consistent but
    incomplete archive (dropped outbox row, replayed publication identity,
    cross-scope manifest, journal resource outside the cut).
    """
    if len(members) > ARCHIVE_MEMBER_BOUND:
        raise ValueError(f"archive member bound: {len(members)}")
    total = 0
    for name, body in members.items():
        if not isinstance(body, bytes):
            raise ValueError(f"member is not bytes: {name}")
        if len(body) > MEMBER_BYTE_BOUND:
            raise ValueError(f"member byte bound: {name}")
        total += len(body)
        if name != "descriptor.json" and not name.startswith(MEMBER_PREFIXES):
            raise ValueError(f"member outside the archive layout: {name}")
    if total > TOTAL_UNCOMPRESSED_BOUND:
        raise ValueError(f"uncompressed total bound: {total}")
    descriptor = json.loads(members["descriptor.json"].decode("utf-8"))
    if canonical(descriptor) != members["descriptor.json"] or set(descriptor) != set(DESCRIPTOR_FIELDS):
        raise ValueError("noncanonical or incomplete descriptor")
    if descriptor["format"] != "online-package-backup-v1" or descriptor["schema_version"] != 2:
        raise ValueError("descriptor format/schema mismatch")
    receipts, current = descriptor["receipts"], descriptor["current"]
    journal, outbox = descriptor["journal"], descriptor["outbox"]
    _check_generations(receipts, current)
    _check_identity_uniqueness(outbox, "outbox")
    _check_identity_uniqueness(journal, "journal")
    for row in receipts:
        if set(row) != set(RECEIPT_ROW_FIELDS):
            raise ValueError("receipt row shape mismatch")
    receipt_map = {(row["tenant"], row["environment"], row["key"]): row for row in receipts}
    for row in current:
        identity = (row.get("tenant"), row.get("environment"), row.get("key"))
        if identity not in receipt_map:
            raise ValueError("current row has no historical receipt in the cut")
    manifests = {}
    manifest_objects = {}
    objects = {}
    for name in members:
        if name.startswith("manifests/"):
            digest = name[len("manifests/"):-len(".json")]
            body = members[name]
            if sha(body) != digest:
                raise ValueError(f"manifest name/hash mismatch: {name}")
            manifest = json.loads(body.decode("utf-8"))
            if canonical(manifest) != body:
                raise ValueError(f"noncanonical manifest: {name}")
            manifests[digest] = body
            manifest_objects[digest] = manifest
        elif name.startswith("objects/"):
            digest = name[len("objects/"):]
            if sha(members[name]) != digest:
                raise ValueError(f"object name/hash mismatch: {name}")
            objects[digest] = members[name]
    for receipt in receipts:
        digest = receipt["manifest_sha256"]
        if digest not in manifests:
            raise ValueError(f"receipt references an absent manifest: {digest}")
    _check_receipt_manifest_scope(receipts, manifest_objects)
    _check_journal_closure(journal, manifests)
    for digest, manifest in manifest_objects.items():
        for package in manifest.get("packages", []):
            blob = package.get("sha256")
            if blob not in objects:
                raise ValueError(f"manifest {digest} references an absent object: {blob}")
    for index, row in enumerate(journal):
        name = f"journal/{index:06d}.json"
        if name not in members or members[name] != canonical(row):
            raise ValueError(f"journal member missing or mismatched: {name}")
    for index, row in enumerate(outbox):
        name = f"outbox/{index:06d}.json"
        if name not in members or members[name] != canonical(row):
            raise ValueError(f"outbox member missing or mismatched: {name}")
        body = row.get("body")
        if not isinstance(body, str):
            raise ValueError("outbox body is not text")
    for row in current:
        name = (Path("deployments") / row["tenant"] / row["environment"] / "current.json").as_posix()
        if name not in members or members[name] != canonical(row):
            raise ValueError(f"current pointer missing or mismatched: {name}")
    if len(members) != 1 + len(manifests) + len(objects) + len(journal) + len(outbox) + len(current):
        raise ValueError("member count does not match the descriptor closure")
    return descriptor


def summary(descriptor, members):
    return dict(cut_id=sha(members["descriptor.json"]),
                scopes=len(descriptor["current"]), receipts=len(descriptor["receipts"]),
                manifests=sum(name.startswith("manifests/") for name in members),
                objects=sum(name.startswith("objects/") for name in members),
                outbox=len(descriptor["outbox"]))


def check_archive(archive, expected_members, expected_summary):
    """The archive must be canonical, bounded, structurally complete, and equal
    to the oracle's own cut."""
    archive = Path(archive)
    data = archive.read_bytes()
    if len(data) > ARCHIVE_BYTE_BOUND or len(data) < 22:
        raise ValueError("archive size bound")
    observed = {}
    with zipfile.ZipFile(archive) as z:
        infos = z.infolist()
        names = [info.filename for info in infos]
        if names != sorted(set(names)):
            raise ValueError("duplicate or unordered archive members")
        if len(names) > ARCHIVE_MEMBER_BOUND:
            raise ValueError(f"archive member bound: {len(names)}")
        for info in infos:
            if info.compress_type != zipfile.ZIP_STORED or info.date_time != (1980, 1, 1, 0, 0, 0):
                raise ValueError("noncanonical ZIP metadata")
            if info.create_system != 3 or info.external_attr != (0o100644 << 16):
                raise ValueError("unsafe ZIP metadata")
            if info.file_size > MEMBER_BYTE_BOUND or info.extra or info.comment or info.flag_bits & 1:
                raise ValueError("ZIP bound or link violation")
            observed[info.filename] = z.read(info)
    validate_members(observed)
    if observed != expected_members:
        raise ValueError("archive members/order mismatch")
    descriptor = json.loads(expected_members["descriptor.json"])
    if summary(descriptor, expected_members) != expected_summary:
        raise ValueError("summary mismatch")


def check_restored(destination, expected_members, expected_summary):
    """The restored repository must be exactly the cut: same authority, same
    member bytes, no additional file, no link."""
    destination = Path(destination)
    descriptor, members = repository_cut(destination)
    if summary(descriptor, members) != expected_summary:
        raise ValueError("restored summary mismatch")
    if members != expected_members:
        raise ValueError("restored authority differs from the cut")
    allowed = set(expected_members) | {RESTORED_DATABASE, *RESTORED_SIDECARS}
    observed = bytes_map(destination)
    for name, entry in observed.items():
        if entry[0] == "directory":
            continue
        if entry[0] != "file":
            raise ValueError(f"link/reparse point in restored destination: {name}")
        if name not in allowed:
            raise ValueError(f"extra file in restored destination: {name}")
    for name, body in expected_members.items():
        if name == "descriptor.json":
            continue
        if observed.get(name) != ("file", sha(body)):
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
    import tempfile
    with tempfile.TemporaryDirectory() as raw:
        root = Path(raw) / "repository"
        request = dict(tenant="tenant-a", environment="prod", key="seed",
                       requirements={"pkg-000": {"min": 1, "max": 2}})
        result = invoke(work, "install", root=str(root), **request)
        if not result.get("ok"):
            raise AssertionError(result)
        descriptor, members = repository_cut(root)
        expected = summary(descriptor, members)
        validate_members(members)
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
        return dict(summary=expected, rejected_mutations=rejected,
                    member_count=len(members),
                    member_bound=ARCHIVE_MEMBER_BOUND,
                    uncompressed_bytes=sum(len(body) for body in members.values()))
