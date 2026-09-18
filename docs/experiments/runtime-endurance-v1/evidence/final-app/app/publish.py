
import json
import sqlite3
import urllib.request


class Outbox:
    def __init__(self, path):
        self.db = sqlite3.connect(path)
        self.db.execute("CREATE TABLE IF NOT EXISTS outbox(id TEXT PRIMARY KEY, body TEXT, status TEXT)")
        self.db.commit()

    def add(self, identity, body):
        self.db.execute("INSERT OR IGNORE INTO outbox VALUES(?,?,?)", (identity, json.dumps(body, sort_keys=True), "pending"))
        self.db.commit()

    def pending(self):
        return self.db.execute("SELECT id,body FROM outbox WHERE status='pending' ORDER BY id").fetchall()

    def close(self):
        self.db.close()


def publish(url, identity, body):
    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Content-Type": "application/json", "Idempotency-Key": identity})
    with urllib.request.urlopen(request, timeout=5) as response:
        return response.read()
