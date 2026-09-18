import json
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys

WORK = Path(__file__).resolve().parent / "workspace"
sys.path.insert(0, str(WORK))
from app.outbox import Outbox


class ReceiverState:
    def __init__(self):
        self.lock = threading.Lock()
        self.accepted = {}
        self.drop_next = True


def main():
    state = ReceiverState()

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_POST(self):  # noqa: N802
            key = self.headers.get("Idempotency-Key")
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            with state.lock:
                old = state.accepted.get(key)
                if old is not None and old != body:
                    self.send_response(409)
                    self.end_headers()
                    return
                state.accepted[key] = body
                if state.drop_next:
                    state.drop_next = False
                    self.close_connection = True
                    return
            self.send_response(201)
            self.end_headers()
            self.wfile.write(b"accepted")

        def do_GET(self):  # noqa: N802
            with state.lock:
                rows = [{"id": key, "accepted": True, "detail": "receiver-audit"} for key in sorted(state.accepted)]
            body = json.dumps(rows).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory() as temp:
            box = Outbox(Path(temp) / "outbox.sqlite")
            identity = "tenant-a/destination-a/receipt-1"
            box.enqueue(identity, {"manifest": "m1"})
            url = f"http://127.0.0.1:{server.server_port}/publish"
            first = box.publish_pending(url, timeout=2)
            second = box.publish_pending(url, timeout=2)
            receipts = box.reconcile(f"http://127.0.0.1:{server.server_port}/receipts", timeout=2)
            result = {"first": first, "second": second, "receipts": [r.__dict__ for r in receipts], "status": box.status(identity)}
            assert first[0]["status"] == "pending"
            assert second[0]["status"] == "sent"
            assert receipts[0].accepted and result["status"]["status"] == "confirmed"
            print(json.dumps(result, indent=2, sort_keys=True))
            (Path(temp) / "receiver-receipt.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
