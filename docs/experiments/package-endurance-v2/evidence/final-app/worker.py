"""Worker process: read an ordered JSONL job file, run installs sequentially.

Each job is a JSON object describing an install request. For every job we
write exactly one JSONL outcome line to the result file and flush it so a
crash never silently loses acknowledged work.

Usage:
    python -m app.worker --root <dir> --catalog <catalog.json> \
        --jobs <jobs.jsonl> --result <result.jsonl> [--jobs-only]

Every worker process owns an independent Repository instance; no state is
shared across processes.
"""
from __future__ import annotations

import argparse
import json
import os
import sys


def _read_jobs(path):
    jobs = []
    with open(path, "r", encoding="utf-8") as fh:
        for lineno, raw in enumerate(fh, 1):
            line = raw.strip()
            if not line:
                continue
            try:
                jobs.append(json.loads(line))
            except Exception as exc:  # malformed job line -> recorded as error
                jobs.append({"__malformed__": True, "line": lineno,
                             "error": str(exc)})
    return jobs


def _outcome(request_id, ok, payload):
    rec = {"request": request_id, "ok": bool(ok)}
    if ok:
        rec["receipt"] = payload
    else:
        rec["error"] = payload
    return rec


def run_jobs(root, catalog_path, jobs_path, result_path, max_jobs=None):
    from app.repository import Repository

    jobs = _read_jobs(jobs_path)
    if max_jobs is not None and max_jobs >= 0:
        jobs = jobs[:max_jobs]

    repo = Repository(root, catalog_path)
    written = 0
    with open(result_path, "w", encoding="utf-8") as out:
        for job in jobs:
            if job.get("__malformed__"):
                out.write(json.dumps(
                    _outcome(None, False, "malformed job line %r: %s"
                             % (job.get("line"), job.get("error"))),
                    sort_keys=True) + "\n")
                out.flush()
                os.fsync(out.fileno())
                written += 1
                continue
            request_id = job.get("request")
            try:
                receipt = repo.install(job)
                rec = _outcome(request_id, True, receipt)
            except Exception as exc:  # per-job isolation: one failure per line
                rec = _outcome(request_id, False, "%s: %s"
                               % (type(exc).__name__, exc))
            out.write(json.dumps(rec, sort_keys=True) + "\n")
            out.flush()
            os.fsync(out.fileno())
            written += 1
    return written


def main(argv=None):
    ap = argparse.ArgumentParser(prog="app.worker")
    ap.add_argument("--root", required=True,
                    help="deployment root directory")
    ap.add_argument("--catalog", required=True,
                    help="path to the immutable package catalog JSON")
    ap.add_argument("--jobs", required=True,
                    help="path to an ordered JSONL job file")
    ap.add_argument("--result", required=True,
                    help="path to the JSONL result file to write")
    ap.add_argument("--max-jobs", type=int, default=None,
                    help="process only the first N jobs (crash testing)")
    args = ap.parse_args(argv)
    run_jobs(args.root, args.catalog, args.jobs, args.result, args.max_jobs)
    return 0


if __name__ == "__main__":
    sys.exit(main())
