"""Bounded, read-only diagnostics for the TB7 protocol.

This broker intentionally exposes a tiny fixed query set. It never prints
environment values, evaluates code, invokes a shell, writes files, or accepts
an arbitrary command.
"""

from __future__ import annotations

import importlib.util
import os
import socket
import sys
from pathlib import Path


ALLOWED_MODULES = (
    "pymysql",
    "psycopg",
    "psycopg2",
    "sqlalchemy",
    "redis",
    "fastapi",
    "gunicorn",
)
ALLOWED_ENDPOINTS = (
    ("main", 8080),
    ("mysql-db", 3306),
    ("postgres-db", 5432),
    ("redis", 6379),
)


def main(argv: list[str]) -> int:
    query = argv[1] if len(argv) > 1 else "help"
    if len(argv) > 2:
        print("read-probe: extra arguments are refused", file=sys.stderr)
        return 2
    if query == "env-names":
        print("\n".join(sorted(os.environ)))
        return 0
    if query == "modules":
        for name in ALLOWED_MODULES:
            print(f"{name}={bool(importlib.util.find_spec(name))}")
        return 0
    if query == "files":
        root = Path("/app")
        paths = sorted(
            str(path.relative_to(root))
            for path in root.rglob("*")
            if path.is_file() and ".git" not in path.parts
        )
        print("\n".join(paths[:128]))
        print(f"count={len(paths)}")
        return 0
    if query == "ports":
        for host, port in ALLOWED_ENDPOINTS:
            try:
                with socket.create_connection((host, port), timeout=2):
                    state = "open"
            except OSError:
                state = "closed"
            print(f"{host}:{port}={state}")
        return 0
    if query == "python":
        print(sys.version.split()[0])
        return 0
    print("queries: env-names modules files ports python", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
