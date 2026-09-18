
import sqlite3


def ensure_v1(path):
    db = sqlite3.connect(path)
    version = db.execute("PRAGMA user_version").fetchone()[0]
    if version == 0:
        db.execute("CREATE TABLE IF NOT EXISTS manifests(id TEXT PRIMARY KEY, digest TEXT NOT NULL)")
        db.execute("PRAGMA user_version=1")
        db.commit()
    elif version != 1:
        raise ValueError(f"unsupported schema version {version}")
    db.close()
