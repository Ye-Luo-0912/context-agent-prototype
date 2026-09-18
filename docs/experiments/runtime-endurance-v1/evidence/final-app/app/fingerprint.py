"""Content and configuration fingerprints.

A *fingerprint* is a stable identifier for the inputs of a build step. The
platform uses two flavours, both deterministic and computed without touching
the network:

``content_fingerprint``
    Hash of the normalised *content* of a set of rows plus the identity of the
    schema that produced them.

``config_fingerprint``
    Hash of the plan/configuration that consumes those rows, including operator
    identity, parameters and the ordered list of upstream fingerprints.

Combining the two gives a ``node_fingerprint`` which is what makes incremental
builds possible: if a node's fingerprint is unchanged from a previous run, its
output cannot have changed and the node can be skipped.
"""

from __future__ import annotations

import hashlib
import json
from typing import Any, Dict, Iterable, List, Sequence

from .engine import compile_plan, result_digest

__all__ = [
    "canonical_bytes",
    "content_fingerprint",
    "config_fingerprint",
    "node_fingerprint",
    "fingerprint_graph",
]


def canonical_bytes(value: Any) -> bytes:
    """Canonical JSON encoding used everywhere a hash is taken."""
    return json.dumps(value, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def content_fingerprint(rows: Sequence[dict], *, schema_id: str | None = None) -> str:
    """Fingerprint the content of ``rows`` in a deterministic order."""
    normalised = {
        "rows": list(rows),
        "schema": schema_id,
    }
    return _sha256(canonical_bytes(normalised))


def config_fingerprint(plan: Sequence[dict]) -> str:
    """Fingerprint the plan structure after compilation."""
    compiled = compile_plan(plan)
    return _sha256(canonical_bytes(compiled))


def node_fingerprint(
    *,
    op: str,
    params: Dict[str, Any],
    upstream: Sequence[str],
    schema_id: str | None,
) -> str:
    """Combine operator, parameters, ordered upstream fingerprints and schema."""
    body = {
        "op": op,
        "params": params,
        "upstream": list(upstream),
        "schema": schema_id,
    }
    return _sha256(canonical_bytes(body))


def fingerprint_graph(rows: Sequence[dict], plan: Sequence[dict], *, schema_id: str | None = None):
    """Compute a fingerprint per node.

    Inputs are hashed by content; each node's fingerprint folds in its operator
    params and the fingerprints of the nodes it depends on, giving a Merkle
    style DAG fingerprint. Returns ``{node_id: fingerprint}`` plus the terminal
    node id under the ``"__terminal__"`` key.
    """
    compiled = compile_plan(plan)
    fingerprints: Dict[str, str] = {}
    root = content_fingerprint(rows, schema_id=schema_id)
    fingerprints["input"] = root
    for node in compiled:
        nid = node["id"]
        if node.get("inputs") is not None:
            deps = list(node["inputs"])
        elif node.get("input") is not None:
            deps = [node["input"]]
        else:
            deps = ["input"]
        params = {k: v for k, v in node.items() if k not in {"id", "op", "input", "inputs"}}
        fingerprints[nid] = node_fingerprint(
            op=node["op"],
            params=params,
            upstream=[fingerprints.get(d, root) for d in deps],
            schema_id=schema_id,
        )
    fingerprints["__terminal__"] = compiled[-1]["id"] if compiled else "input"
    return fingerprints
