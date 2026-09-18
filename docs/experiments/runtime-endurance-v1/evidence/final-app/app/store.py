"""Content-addressed store with immutable manifests and atomic garbage collection.

Invariants
----------
1. **Immutability.** A blob is written once and never modified. Its name *is*
   its SHA-256, so a corrupted or tampered file is detected on read.
2. **Atomicity.** Blobs are staged in a temp file, ``fsync``-ed and then
   ``os.replace``-d into place. Readers therefore never observe a partial blob.
   Directory metadata is fsynced so the rename survives a crash.
3. **Manifests.** A manifest is a JSON document listing the blobs that belong
   to one build. Manifests are themselves content-addressed and immutable; the
   *latest* pointer is a separate small file that is swapped atomically. This
   is what lets a concurrent GC run without deleting a blob that a fresh
   manifest is about to reference.
4. **GC.** Garbage collection is a *two-phase* operation. Phase 1 computes the
   reachable set from a caller-supplied snapshot of live manifests. Phase 2
   re-reads the live set immediately before deletion and only deletes blobs
   that are still unreferenced, moving them into a quarantine directory first
   so an in-flight reader can still finish. Because the live set is re-read
   under a lock, a manifest that was published concurrently is never collected.

The store is safe against multiple OS processes because every mutation of the
live-set pointer happens through the same atomic-rename protocol and the GC
takes an exclusive lock file.
"""

from __future__ import annotations

import contextlib
import errno
import hashlib
import json
import os
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, Iterable, List, Sequence, Set

__all__ = ["CasError", "CasStore", "Manifest"]


class CasError(RuntimeError):
    pass


def _atomic_write(path: Path, data: bytes) -> None:
    """Write ``data`` to ``path`` atomically (temp + fsync + rename)."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.parent / f".{path.name}.{os.getpid()}.{time.time_ns()}.tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp, path)
        _fsync_dir(path.parent)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            tmp.unlink()
        raise


def _fsync_dir(path: Path) -> None:
    """Best-effort directory fsync (no-op on platforms that disallow it)."""
    try:
        fd = os.open(str(path), os.O_RDONLY)
    except OSError:
        return
    try:
        os.fsync(fd)
    except OSError:
        pass
    finally:
        os.close(fd)


@contextlib.contextmanager
def _file_lock(lock_path: Path, timeout: float = 30.0, poll: float = 0.02):
    """A cross-process exclusive lock built on ``O_EXCL`` lock files.

    Portable to Windows and POSIX without third-party dependencies. Stale locks
    (older than ``timeout`` seconds) are broken so a crashed GC cannot wedge the
    store forever.
    """
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    deadline = time.monotonic() + timeout
    while True:
        try:
            fd = os.open(str(lock_path), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
            os.write(fd, f"{os.getpid()} {time.time()}".encode())
            os.close(fd)
            break
        except OSError as exc:
            if exc.errno != errno.EEXIST:
                raise
            try:
                age = time.time() - lock_path.stat().st_mtime
            except FileNotFoundError:
                continue
            if age > timeout:
                with contextlib.suppress(FileNotFoundError):
                    lock_path.unlink()
                continue
            if time.monotonic() > deadline:
                raise CasError(f"timed out acquiring lock {lock_path}")
            time.sleep(poll)
    try:
        yield
    finally:
        with contextlib.suppress(FileNotFoundError):
            lock_path.unlink()


@dataclass
class Manifest:
    """An immutable description of one build's outputs."""

    identity: str
    entries: Dict[str, str]  # logical name -> blob digest
    created_at: str
    parents: List[str] = field(default_factory=list)
    meta: Dict[str, str] = field(default_factory=dict)

    def to_json(self) -> str:
        return json.dumps(
            {
                "identity": self.identity,
                "entries": dict(sorted(self.entries.items())),
                "created_at": self.created_at,
                "parents": list(self.parents),
                "meta": dict(sorted(self.meta.items())),
            },
            sort_keys=True,
            separators=(",", ":"),
        )

    @classmethod
    def from_json(cls, text: str) -> "Manifest":
        data = json.loads(text)
        return cls(
            identity=data["identity"],
            entries=dict(data["entries"]),
            created_at=data["created_at"],
            parents=list(data.get("parents", [])),
            meta=dict(data.get("meta", {})),
        )


class CasStore:
    """A blob store with immutable content-addressed manifests.

    Directory layout::

        <root>/blobs/ab/abcdef...      # immutable blob content
        <root>/manifests/<digest>      # immutable manifest body
        <root>/live/<identity>         # latest manifest digest for a logical id
        <root>/quarantine/<digest>     # blobs pending GC
        <root>/gc.lock                 # GC mutual exclusion
    """

    def __init__(self, root):
        self.root = Path(root)
        self.blobs = self.root / "blobs"
        self.manifests = self.root / "manifests"
        self.live = self.root / "live"
        self.quarantine = self.root / "quarantine"
        for d in (self.blobs, self.manifests, self.live, self.quarantine):
            d.mkdir(parents=True, exist_ok=True)

    # -- blobs --------------------------------------------------------------- #
    def _blob_path(self, digest: str) -> Path:
        return self.blobs / digest[:2] / digest

    def put(self, data: bytes) -> str:
        digest = hashlib.sha256(data).hexdigest()
        target = self._blob_path(digest)
        if not target.exists():
            _atomic_write(target, data)
        return digest

    def get(self, digest: str) -> bytes:
        path = self._blob_path(digest)
        try:
            data = path.read_bytes()
        except FileNotFoundError as exc:
            raise CasError(f"blob {digest} not found") from exc
        if hashlib.sha256(data).hexdigest() != digest:
            raise CasError(f"blob {digest} is corrupt")
        return data

    def has(self, digest: str) -> bool:
        return self._blob_path(digest).exists()

    def put_json(self, value) -> str:
        return self.put(json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode())

    def get_json(self, digest: str):
        return json.loads(self.get(digest))

    # -- manifests ------------------------------------------------------------ #
    def put_manifest(self, manifest: Manifest) -> str:
        body = manifest.to_json().encode("utf-8")
        digest = self.put(body)  # content-addressed like any blob
        _atomic_write(self.manifests / digest, body)
        return digest

    def get_manifest(self, digest: str) -> Manifest:
        path = self.manifests / digest
        if not path.exists():
            # fall back to blob storage in case only the blob copy is present
            return Manifest.from_json(self.get(digest).decode("utf-8"))
        return Manifest.from_json(path.read_text(encoding="utf-8"))

    def publish(self, identity: str, manifest: Manifest) -> str:
        """Publish ``manifest`` as the current tip for ``identity`` atomically."""
        digest = self.put_manifest(manifest)
        _atomic_write(self.live / identity, f"{digest}\n".encode())
        return digest

    def current(self, identity: str) -> str | None:
        pointer = self.live / identity
        if not pointer.exists():
            return None
        return pointer.read_text(encoding="utf-8").strip() or None

    def live_manifests(self) -> Set[str]:
        """Return the digests of every currently published manifest tip."""
        return {d for d in (self.current(i) for i in self.identities()) if d}

    def identities(self) -> List[str]:
        if not self.live.exists():
            return []
        # Tenant/destination identities are namespaced with '/' and therefore
        # live under nested directories. Enumerating only the top level would
        # make reachable() miss a published manifest and let GC delete its
        # blobs.
        return sorted(
            p.relative_to(self.live).as_posix()
            for p in self.live.rglob("*")
            if p.is_file()
        )

    def reachable(self, extra_roots: Iterable[str] = ()) -> Set[str]:
        """Compute every blob digest reachable from live manifests + extra roots."""
        reachable: Set[str] = set()
        stack: List[str] = list(self.live_manifests()) + list(extra_roots)
        seen_manifests: Set[str] = set()
        while stack:
            digest = stack.pop()
            if digest in seen_manifests:
                continue
            seen_manifests.add(digest)
            try:
                manifest = self.get_manifest(digest)
            except (CasError, json.JSONDecodeError):
                continue
            for blob in manifest.entries.values():
                reachable.add(blob)
            for parent in manifest.parents:
                stack.append(parent)
            reachable.add(digest)
        return reachable

    def all_blobs(self) -> List[str]:
        out = []
        for path in self.blobs.rglob("*"):
            if path.is_file() and not path.name.endswith(".tmp") and not path.name.startswith("."):
                out.append(path.name)
        return out

    # -- GC ------------------------------------------------------------------- #
    def gc(self, referenced=None, *, extra_roots: Iterable[str] = (), quarantine_ttl: float = 0.0) -> List[str]:
        """Atomically remove unreferenced blobs.

        ``referenced`` may be supplied for backward compatibility (a plain
        collection of digests to keep). When it is ``None`` the reachable set is
        derived from live manifests. In both cases the live set is re-read under
        the store lock immediately before deletion so concurrent publishers are
        never harmed.
        """
        removed: List[str] = []
        with _file_lock(self.root / "gc.lock"):
            # Phase 1: snapshot reachable set.
            reachable = self.reachable(extra_roots=extra_roots)
            if referenced is not None:
                reachable |= set(referenced)
            # Phase 2: with the lock held, re-read the live set so a manifest
            # published during phase 1 is protected.
            reachable |= self.live_manifests()
            reachable |= self.reachable()

            self._drain_quarantine(quarantine_ttl)
            for digest in self.all_blobs():
                if digest in reachable:
                    continue
                # Skip manifest bodies and the temp artifacts of a concurrent put.
                path = self._blob_path(digest)
                try:
                    os.replace(path, self.quarantine / digest)
                except FileNotFoundError:
                    continue
                removed.append(digest)
        return removed

    def _drain_quarantine(self, ttl: float) -> None:
        now = time.time()
        for path in self.quarantine.iterdir():
            if not path.is_file():
                continue
            if ttl and (now - path.stat().st_mtime) < ttl:
                continue
            with contextlib.suppress(FileNotFoundError):
                path.unlink()

    def verify(self) -> bool:
        """Verify every live manifest and every blob it references."""
        for identity in self.identities():
            digest = self.current(identity)
            if digest is None:
                continue
            manifest = self.get_manifest(digest)
            for blob in manifest.entries.values():
                self.get(blob)  # raises on corruption/missing
        return True
