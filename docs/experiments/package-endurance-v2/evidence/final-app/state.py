"""SQLite authority: schema, journal, receipts/current helpers (SPEC.md)."""
import os
import sqlite3

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
    body TEXT NOT NULL, status TEXT NOT NULL, ack TEXT,
    PRIMARY KEY(tenant, environment, key));
"""


SCHEMA_VERSION = 2


def connect(root):
    path = os.path.join(os.fspath(root), 'repo.sqlite')
    conn = sqlite3.connect(path, isolation_level=None, timeout=30.0)
    conn.row_factory = sqlite3.Row
    # Refuse to touch a database written by a future implementation before
    # changing journal_mode or running any DDL against it.
    version = conn.execute('PRAGMA user_version').fetchone()[0]
    if version > SCHEMA_VERSION:
        conn.close()
        raise ValueError('unsupported future schema version %r' % (version,))
    conn.execute('PRAGMA journal_mode=WAL')
    conn.execute('PRAGMA synchronous=FULL')
    return conn


def begin(conn):
    conn.execute('BEGIN IMMEDIATE')


def commit(conn):
    conn.execute('COMMIT')


def rollback(conn):
    try:
        conn.execute('ROLLBACK')
    except sqlite3.OperationalError:
        pass


def ensure_schema(conn):
    version = conn.execute('PRAGMA user_version').fetchone()[0]
    if version > SCHEMA_VERSION:
        raise ValueError('unsupported future schema version %r' % (version,))
    conn.executescript(SCHEMA)
    if version == 0:
        conn.execute('PRAGMA user_version=%d' % SCHEMA_VERSION)
        version = SCHEMA_VERSION
    return version


def auth_version(conn):
    return conn.execute('PRAGMA user_version').fetchone()[0]


def read_receipt(conn, tenant, environment, key):
    return conn.execute(
        'SELECT generation, manifest_sha256, request_json FROM receipts'
        ' WHERE tenant=? AND environment=? AND key=?',
        (tenant, environment, key)).fetchone()


def all_receipts(conn):
    return conn.execute('SELECT * FROM receipts').fetchall()


def insert_receipt(conn, tenant, environment, key, generation, manifest_sha256, request_json):
    conn.execute(
        'INSERT INTO receipts(tenant,environment,key,generation,manifest_sha256,request_json)'
        ' VALUES(?,?,?,?,?,?)',
        (tenant, environment, key, generation, manifest_sha256, request_json))


def read_current(conn, tenant, environment):
    return conn.execute(
        'SELECT receipt_json FROM current WHERE tenant=? AND environment=?',
        (tenant, environment)).fetchone()


def put_current(conn, tenant, environment, receipt):
    import json
    conn.execute(
        'INSERT OR REPLACE INTO current(tenant,environment,receipt_json) VALUES(?,?,?)',
        (tenant, environment, json.dumps(receipt, sort_keys=True, separators=(',', ':'),
                                         allow_nan=False)))


def journal_put(conn, tenant, environment, key, row):
    """Upsert a full journal row (dict) keyed by (tenant, environment, key)."""
    cols = ('tenant', 'environment', 'key', 'status', 'old_generation', 'old_manifest',
            'old_receipt', 'new_generation', 'new_manifest', 'new_receipt')
    values = tuple(row.get(c) for c in cols)
    conn.execute(
        'INSERT OR REPLACE INTO journal'
        '(tenant,environment,key,status,old_generation,old_manifest,old_receipt,'
        'new_generation,new_manifest,new_receipt) VALUES(?,?,?,?,?,?,?,?,?,?)',
        values)


def journal_row(conn, tenant, environment, key):
    row = conn.execute(
        'SELECT * FROM journal WHERE tenant=? AND environment=? AND key=?',
        (tenant, environment, key)).fetchone()
    return dict(row) if row is not None else None


def journal_delete(conn, tenant, environment, key):
    conn.execute('DELETE FROM journal WHERE tenant=? AND environment=? AND key=?',
                 (tenant, environment, key))


def journals(conn):
    return conn.execute('SELECT * FROM journal').fetchall()


def put_outbox(conn, tenant, environment, key, body, status, ack=None):
    conn.execute(
        'INSERT OR REPLACE INTO outbox(tenant,environment,key,body,status,ack)'
        ' VALUES(?,?,?,?,?,?)',
        (tenant, environment, key, body, status, ack))


def read_outbox(conn, tenant, environment, key):
    return conn.execute(
        'SELECT body, status, ack FROM outbox WHERE tenant=? AND environment=? AND key=?',
        (tenant, environment, key)).fetchone()
