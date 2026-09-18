"""Programmatic API and command line interface.

The API is the single entry point used by both the CLI and tests. Operations
that touch the filesystem take an explicit *workspace root* so that multiple
builds can run against isolated stores in the same process tree.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, Dict, List, Sequence

from . import engine
from .engine import compare_results, execute_plan, reference_execute, plan_digest, result_digest
from .fingerprint import config_fingerprint, content_fingerprint, fingerprint_graph
from .migration import migrate
from .outbox import Outbox, is_localhost
from .queue import JobQueue
from .schema import parse_csv, parse_jsonl, validate_rows
from .store import CasStore, Manifest
from .workers import MAX_WORKERS, MIN_WORKERS, build_pool

__all__ = ["Platform", "main", "build_from_files"]


class Platform:
    """A build platform rooted at ``root``."""

    def __init__(self, root):
        self.root = Path(root)
        self.store = CasStore(self.root / "cas")
        self.queue = JobQueue(self.root / "jobs.sqlite")
        self.outbox = Outbox(self.root / "outbox.sqlite")
        self.state_path = self.root / "state.json"

    # -- state helpers -------------------------------------------------------- #
    def _state(self) -> Dict[str, Any]:
        if self.state_path.exists():
            return json.loads(self.state_path.read_text(encoding="utf-8"))
        return {"builds": {}}

    def _save_state(self, state: Dict[str, Any]) -> None:
        self.state_path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.state_path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(state, sort_keys=True, indent=2), encoding="utf-8")
        tmp.replace(self.state_path)

    # -- ingestion ------------------------------------------------------------ #
    def load_rows(self, path) -> List[dict]:
        text = Path(path).read_text(encoding="utf-8")
        if str(path).endswith(".csv"):
            return parse_csv(text, strict=True)
        return parse_jsonl(text, strict=True)

    # -- build ---------------------------------------------------------------- #
    def build(
        self,
        identity: str,
        rows: Sequence[dict],
        plan: Sequence[dict],
        *,
        incremental: bool = True,
        check_reference: bool = True,
        schema_id: str | None = None,
    ) -> Dict[str, Any]:
        """Run a build, publishing an immutable manifest keyed by ``identity``.

        Incremental behaviour: if the combined content+config fingerprint equals
        the recorded fingerprint for ``identity`` we short-circuit and reuse the
        previous manifest, proving the output cannot have changed.
        """
        content_fp = content_fingerprint(rows, schema_id=schema_id)
        config_fp = config_fingerprint(plan)
        combined = f"{content_fp}:{config_fp}"

        state = self._state()
        previous = state["builds"].get(identity)
        if incremental and previous and previous.get("fingerprint") == combined:
            manifest_digest = self.store.current(f"build-{identity}")
            return {
                "identity": identity,
                "fingerprint": combined,
                "status": "cached",
                "manifest": manifest_digest,
                "rows": previous.get("result", []),
                "reused": True,
            }

        trace: List[engine.NodeResult] = []
        result = execute_plan(rows, plan, trace=trace)

        if check_reference:
            reference = reference_execute(rows, plan)
            compare_results(result, reference, context=f"build {identity!r}")

        result_body = json.dumps(result, sort_keys=True, ensure_ascii=False).encode("utf-8")
        blob = self.store.put(result_body)
        entries = {"result": blob}
        for node in trace:
            body = json.dumps(node.rows, sort_keys=True, ensure_ascii=False).encode("utf-8")
            entries[f"node:{node.node_id}"] = self.store.put(body)

        parents = [previous["manifest"]] if previous and previous.get("manifest") else []
        manifest = Manifest(
            identity=identity,
            entries=entries,
            created_at=str(time.time()),
            parents=parents,
            meta={
                "fingerprint": combined,
                "content_fingerprint": content_fp,
                "config_fingerprint": config_fp,
                "plan_digest": plan_digest(plan),
                "result_digest": result_digest(result),
            },
        )
        manifest_digest = self.store.publish(f"build-{identity}", manifest)

        state["builds"][identity] = {
            "fingerprint": combined,
            "manifest": manifest_digest,
            "result": result,
        }
        self._save_state(state)

        node_fps = fingerprint_graph(list(rows), plan, schema_id=schema_id)
        return {
            "identity": identity,
            "fingerprint": combined,
            "status": "built",
            "manifest": manifest_digest,
            "rows": result,
            "result_digest": result_digest(result),
            "node_fingerprints": node_fps,
            "reused": False,
        }

    # -- workers -------------------------------------------------------------- #
    def run_jobs(self, jobs: Sequence[Dict[str, Any]], *, workers: int = MIN_WORKERS, crash_after=None):
        for job in jobs:
            self.queue.enqueue(job["id"], job.get("payload", {}), max_attempts=job.get("max_attempts", 3))
        pool = build_pool(self.root / "jobs.sqlite", self.root / "cas", workers=workers)
        pool.start(crash_after=crash_after)
        idle = pool.wait_idle(timeout=30)
        pool.stop()
        return {"idle": idle, "counts": self.queue.counts(), "log": pool.drain_log()}

    # -- publication ---------------------------------------------------------- #
    def publish(self, url: str, messages: Sequence[Dict[str, Any]]):
        for message in messages:
            self.outbox.enqueue(message["id"], message.get("body", {}))
        return self.outbox.publish_pending(url)

    def gc(self):
        return self.store.gc()

    def close(self):
        self.queue.close()


# --------------------------------------------------------------------------- #
# CLI
# --------------------------------------------------------------------------- #

def build_from_files(root, identity, input_path, plan_path, *, incremental=True) -> Dict[str, Any]:
    rows = Path(input_path).read_text(encoding="utf-8")
    path = Path(input_path)
    parsed = parse_csv(rows, strict=True) if path.suffix == ".csv" else parse_jsonl(rows, strict=True)
    plan = json.loads(Path(plan_path).read_text(encoding="utf-8"))
    platform = Platform(root)
    try:
        return platform.build(identity, parsed, plan, incremental=incremental)
    finally:
        platform.close()


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(prog="app.cli", description="Incremental build and publication platform")
    parser.add_argument("--root", default=".platform", help="workspace root for stores (default: .platform)")
    sub = parser.add_subparsers(dest="command", required=True)

    p_build = sub.add_parser("build", help="run a build")
    p_build.add_argument("--identity", required=True)
    p_build.add_argument("--input", required=True)
    p_build.add_argument("--plan", required=True)
    p_build.add_argument("--no-incremental", action="store_true")
    p_build.add_argument("--output")

    p_jobs = sub.add_parser("jobs", help="run queued jobs with worker processes")
    p_jobs.add_argument("--workers", type=int, default=MIN_WORKERS)
    p_jobs.add_argument("--jobs", required=True, help="JSON file: [{id,payload,max_attempts}]")
    p_jobs.add_argument("--crash-after", type=int, default=None)

    p_pub = sub.add_parser("publish", help="publish pending outbox messages")
    p_pub.add_argument("--url", required=True)
    p_pub.add_argument("--messages", help="JSON file: [{id,body}]")

    sub.add_parser("gc", help="run atomic garbage collection")
    sub.add_parser("verify", help="verify CAS integrity and live manifests")

    p_mig = sub.add_parser("migrate", help="migrate the platform database v1 -> v2")
    p_mig.add_argument("--db", required=True)

    args = parser.parse_args(argv)
    root = Path(args.root)

    if args.command == "build":
        result = build_from_files(root, args.identity, args.input, args.plan, incremental=not args.no_incremental)
        if args.output:
            Path(args.output).write_text(json.dumps(result["rows"], indent=2, sort_keys=True), encoding="utf-8")
        print(json.dumps({k: v for k, v in result.items() if k != "rows"}, indent=2, sort_keys=True, default=str))
        return 0

    if args.command == "jobs":
        platform = Platform(root)
        try:
            jobs = json.loads(Path(args.jobs).read_text(encoding="utf-8"))
            out = platform.run_jobs(jobs, workers=args.workers, crash_after=args.crash_after)
        finally:
            platform.close()
        print(json.dumps({"idle": out["idle"], "counts": out["counts"]}, indent=2, sort_keys=True))
        return 0

    if args.command == "publish":
        if not is_localhost(args.url):
            print(json.dumps({"error": "refusing non-localhost target"}), file=sys.stderr)
            return 2
        platform = Platform(root)
        try:
            messages = json.loads(Path(args.messages).read_text(encoding="utf-8")) if args.messages else []
            results = platform.publish(args.url, messages)
        finally:
            platform.close()
        print(json.dumps(results, indent=2, sort_keys=True))
        return 0

    if args.command == "gc":
        platform = Platform(root)
        try:
            removed = platform.gc()
        finally:
            platform.close()
        print(json.dumps({"removed": removed}, indent=2))
        return 0

    if args.command == "verify":
        platform = Platform(root)
        try:
            ok = platform.store.verify()
        finally:
            platform.close()
        print(json.dumps({"ok": ok}))
        return 0 if ok else 1

    if args.command == "migrate":
        version = migrate(args.db)
        print(json.dumps({"user_version": version}))
        return 0

    return 1


if __name__ == "__main__":
    raise SystemExit(main())
