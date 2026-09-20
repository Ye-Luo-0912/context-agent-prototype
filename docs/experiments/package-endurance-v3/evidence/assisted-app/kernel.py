"""SQLite-backed durability kernel co-owned by deploy.transaction (SPEC.md)."""
import json
import os
import sqlite3


class SqliteKernel:
    """Thread-local sqlite connection to root/repo.sqlite with an autocommit cursor."""

    def __init__(self, root):
        self.root = root
        self._conn = None
        self._local = None

    def _connect(self):
        import threading
        self._local = threading.local()
        return self

    def _db(self):
        import threading
        if getattr(self, '_local', None) is None:
            self._local = threading.local()
        conn = getattr(self._local, 'conn', None)
        if conn is None:
            path = os.path.join(os.fspath(self.root), 'repo.sqlite')
            conn = sqlite3.connect(path, isolation_level=None, timeout=30.0)
            conn.execute('PRAGMA journal_mode=WAL')
            conn.execute('PRAGMA synchronous=FULL')
            self._local.conn = conn
        return conn

    def close(self):
        import threading
        loc = getattr(self, '_local', None)
        if loc is not None and getattr(loc, 'conn', None) is not None:
            loc.conn.close()
            loc.conn = None

    def execute(self, sql, params=()):
        return self._db().execute(sql, params)

    def commit(self):
        self._db().execute('COMMIT')

    def begin(self):
        self._db().execute('BEGIN IMMEDIATE')

    def rollback(self):
        try:
            self._db().execute('ROLLBACK')
        except sqlite3.OperationalError:
            pass


def json_canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(',', ':'),
                      allow_nan=False).encode('utf-8')
