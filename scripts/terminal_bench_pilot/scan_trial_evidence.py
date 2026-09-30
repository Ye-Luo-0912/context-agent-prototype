"""Scan explicitly named trial evidence for a stdin-only account key.

Outputs only a file count and a boolean. Never prints or stores the key or
matched bytes; does not follow symlinks or inspect unrelated cache roots.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys


def contains_bytes(path: Path, needle: bytes) -> bool:
    overlap = b""
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            if needle in overlap + chunk:
                return True
            keep = len(needle) - 1
            overlap = (overlap + chunk)[-keep:] if keep else b""
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("roots", type=Path, nargs="+")
    args = parser.parse_args()
    key = sys.stdin.readline().rstrip("\r\n").encode()
    if not key or len(key) > 512:
        raise RuntimeError("missing or invalid in-memory scan credential")
    files = [path for root in args.roots for path in root.rglob("*")
             if path.is_file() and not path.is_symlink()]
    found = any(contains_bytes(path, key) for path in files)
    print(json.dumps({"files_scanned": len(files),
                      "account_credential_present": found}), flush=True)
    return 1 if found else 0


if __name__ == "__main__":
    raise SystemExit(main())
