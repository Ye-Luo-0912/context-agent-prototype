# Incremental Build and Publication Platform v1

This is a deliberately small runnable v1. The next task will upgrade it across
the modules below. Do not modify TASK.md, tests, fixtures or oracle.py.

The v1 contract is intentionally incomplete: it supports a basic JSONL import,
a small DAG evaluator, local CAS blobs, one SQLite queue and a local publication
stub. Upgrade requests will add strict schemas, content fingerprints, worker
fencing, atomic manifests, an outbox, migration and recovery documentation.
