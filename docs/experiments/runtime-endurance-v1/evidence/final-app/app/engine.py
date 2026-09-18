"""Deterministic DAG evaluator plus an independent reference implementation.

Two things matter here and they are deliberately kept separate:

``referenced`` operators
    A hand-written, dependency-free implementation of every operator that the
    reference checker uses. It is intentionally written in the most obvious way
    possible (Python built-ins, no shared helper code with the engine) so that a
    bug in the engine's optimised path cannot mask itself.

``execute_plan``
    The production evaluator. It uses the same operator semantics but is
    structured for reuse inside worker processes and records a deterministic
    trace per node so that two runs of the same plan can be compared.

Everything is *pure*: operators never read the network, never spawn processes
and never ``eval`` payloads. Operators only transform the in-memory row lists.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, Iterable, List, Sequence

from .schema import Schema, parse_jsonl, parse_csv, validate_rows

__all__ = [
    "PlanError",
    "OperatorError",
    "NodeResult",
    "register",
    "OPERATORS",
    "compile_plan",
    "execute_plan",
    "reference_execute",
    "compare_results",
    "read_jsonl",
    "write_json",
    "plan_digest",
]


class PlanError(ValueError):
    """Raised for structural problems in a plan (missing node, cycle, ...)."""


class OperatorError(ValueError):
    """Raised when an operator is applied to incompatible input."""


# --------------------------------------------------------------------------- #
# Operators. Each takes (rows, node, resolve) -> rows.
# --------------------------------------------------------------------------- #

OPERATORS: Dict[str, Any] = {}


def register(name):
    def deco(fn):
        OPERATORS[name] = fn
        return fn

    return deco


def _require(node: dict, key: str):
    if key not in node:
        raise OperatorError(f"node {node.get('id')!r} requires key {key!r}")
    return node[key]


@register("select")
def _op_select(rows, node, resolve):
    columns = _require(node, "columns")
    if not isinstance(columns, list) or not all(isinstance(c, str) for c in columns):
        raise OperatorError("select.columns must be a list of strings")
    missing = [c for c in columns if rows and c not in rows[0]]
    if missing and not node.get("optional"):
        raise OperatorError(f"select references unknown columns {missing!r}")
    if node.get("strict_columns"):
        for row in rows:
            if set(row) - set(columns):
                raise OperatorError("select.strict_columns forbids dropping nothing")
    return [{k: row[k] for k in columns if k in row} for row in rows]


@register("filter")
def _op_filter(rows, node, resolve):
    if "key" not in node:
        raise OperatorError("filter requires key")
    key = node["key"]
    if "value" in node:
        target = node["value"]
        return [r for r in rows if r.get(key) == target]
    if "in" in node:
        allowed = node["in"]
        if not isinstance(allowed, list):
            raise OperatorError("filter.in must be a list")
        return [r for r in rows if r.get(key) in allowed]
    if "predicate" in node:
        pred = node["predicate"]
        if not isinstance(pred, dict) or len(pred) != 1:
            raise OperatorError("filter.predicate must be a single operator mapping")
        (op, arg), = pred.items()
        if op in {"lt", "le", "gt", "ge", "eq", "ne"}:
            import operator as _op

            fn = {"lt": _op.lt, "le": _op.le, "gt": _op.gt, "ge": _op.ge, "eq": _op.eq, "ne": _op.ne}[op]
            return [r for r in rows if key in r and fn(r[key], arg)]
        raise OperatorError(f"unsupported predicate {op!r}")
    raise OperatorError("filter requires value, in or predicate")


@register("rename")
def _op_rename(rows, node, resolve):
    mapping = node.get("mapping", {})
    if not isinstance(mapping, dict):
        raise OperatorError("rename.mapping must be a mapping")
    out = []
    for row in rows:
        renamed = {mapping.get(k, k): v for k, v in row.items()}
        if len(renamed) != len(row):
            raise OperatorError("rename produced colliding column names")
        out.append(renamed)
    return out


@register("sort")
def _op_sort(rows, node, resolve):
    keys = node.get("keys")
    if keys is None:
        keys = [_require(node, "key")]
    if not isinstance(keys, list):
        raise OperatorError("sort.keys must be a list")
    descending = bool(node.get("descending"))
    strict = bool(node.get("strict", True))
    for k in keys:
        for row in rows:
            if k not in row:
                if strict:
                    raise OperatorError(f"sort key {k!r} missing from a row")
    # Python's sort is stable; we additionally break ties on the full row's
    # canonical JSON so that equal-key ordering is fully deterministic even if
    # the input order differs.
    def sort_key(row):
        primary = tuple(row.get(k) for k in keys)
        tiebreak = json.dumps(row, sort_keys=True, ensure_ascii=False)
        return (primary, tiebreak)

    try:
        result = sorted(rows, key=sort_key, reverse=descending)
    except TypeError as exc:  # mixed types in a key column
        raise OperatorError(f"sort key types are not comparable: {exc}") from exc
    return result


@register("project")
def _op_project(rows, node, resolve):
    return _op_select(rows, node, resolve)


@register("concat")
def _op_concat(rows, node, resolve):
    inputs = _require(node, "inputs")
    combined: List[dict] = []
    for name in inputs:
        combined.extend(resolve(name))
    return combined


@register("distinct")
def _op_distinct(rows, node, resolve):
    seen = set()
    out = []
    for row in rows:
        marker = json.dumps(row, sort_keys=True, ensure_ascii=False)
        if marker not in seen:
            seen.add(marker)
            out.append(row)
    return out


@register("limit")
def _op_limit(rows, node, resolve):
    n = int(_require(node, "count"))
    return rows[:n]


@register("export")
def _op_export(rows, node, resolve):
    return list(rows)


# --------------------------------------------------------------------------- #
# Plan compilation / evaluation
# --------------------------------------------------------------------------- #

def _topological(plan: Sequence[dict]) -> List[dict]:
    by_id = {}
    order: List[str] = []
    for node in plan:
        nid = node.get("id")
        if not isinstance(nid, str) or not nid:
            raise PlanError(f"node missing string id: {node!r}")
        if nid in by_id:
            raise PlanError(f"duplicate node id {nid!r}")
        by_id[nid] = node
        order.append(nid)

    deps: Dict[str, List[str]] = {}
    for nid in order:
        node = by_id[nid]
        raw = node.get("inputs")
        if raw is None and node.get("input") is not None:
            raw = [node["input"]]
        raw = raw or []
        for dep in raw:
            if dep not in by_id:
                raise PlanError(f"node {nid!r} references unknown input {dep!r}")
        deps[nid] = list(raw)

    # Kahn's algorithm; stable by original order.
    indeg = {nid: 0 for nid in order}
    for nid in order:
        for dep in deps[nid]:
            indeg[nid] += 1
    ready = [nid for nid in order if indeg[nid] == 0]
    resolved: List[str] = []
    while ready:
        nid = ready.pop(0)
        resolved.append(nid)
        for other in order:
            if nid in deps[other]:
                indeg[other] -= 1
                if indeg[other] == 0:
                    ready.append(other)
    if len(resolved) != len(order):
        cycle = sorted(set(order) - set(resolved))
        raise PlanError(f"plan contains a cycle among {cycle!r}")
    return [by_id[nid] for nid in resolved]


def compile_plan(plan: Sequence[dict]) -> List[dict]:
    """Validate a plan's structure and return it in deterministic node order."""
    if not isinstance(plan, list):
        raise PlanError("plan must be a list of nodes")
    for node in plan:
        if not isinstance(node, dict):
            raise PlanError("each plan node must be an object")
        op = node.get("op")
        if op not in OPERATORS:
            raise PlanError(f"unsupported operation: {op!r}")
    return _topological(plan)


@dataclass
class NodeResult:
    node_id: str
    op: str
    rows: List[dict]
    digest: str
    row_count: int


def execute_plan(rows, plan, *, trace: List[NodeResult] | None = None):
    """Evaluate ``plan`` against ``rows`` and return the terminal node's rows.

    The result is deterministic: node ordering is topologically sorted with a
    stable tie-break and every operator is a pure function.
    """
    compiled = compile_plan(plan)
    values: Dict[str, List[dict]] = {"input": list(rows)}
    if not compiled:
        return list(rows)

    def resolve(name: str) -> List[dict]:
        if name not in values:
            raise PlanError(f"input {name!r} not produced before use")
        return values[name]

    for node in compiled:
        nid = node["id"]
        inputs = node.get("inputs")
        if inputs is None:
            src_name = node.get("input", "input")
            src = resolve(src_name)
        else:
            src = None
        op = node["op"]
        if inputs is None:
            result = OPERATORS[op](src, node, resolve)
        else:
            # concat-style operators accept multiple named inputs.
            result = OPERATORS[op](None, node, resolve)
        if not isinstance(result, list):
            raise OperatorError(f"operator {op!r} did not return a list")
        values[nid] = result
        if trace is not None:
            trace.append(
                NodeResult(
                    node_id=nid,
                    op=op,
                    rows=result,
                    digest=result_digest(result),
                    row_count=len(result),
                )
            )
    return values[compiled[-1]["id"]]


def result_digest(rows: Sequence[dict]) -> str:
    import hashlib

    payload = json.dumps(list(rows), sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()
    return hashlib.sha256(payload).hexdigest()


# --------------------------------------------------------------------------- #
# Independent reference implementation.
#
# This deliberately shares NO code with the engine above. It re-derives the
# semantics from scratch using the simplest possible constructs so that it can
# act as an oracle for the engine. A divergence means at least one of the two
# is wrong and the build must fail.
# --------------------------------------------------------------------------- #

def reference_execute(rows, plan):
    """A second, independent evaluation of a plan.

    Supports the same operator set but implemented inline without the registry,
    without dataclasses and without the trace machinery.
    """
    id_to_node = {}
    for node in plan:
        id_to_node[node["id"]] = node

    done = {}
    done["input"] = [dict(r) for r in rows]

    remaining = list(plan)
    progress = True
    while remaining and progress:
        progress = False
        still = []
        for node in remaining:
            need = node.get("inputs")
            if need is None:
                if "input" in node:
                    need = [node["input"]]
                elif node["op"] == "concat":
                    need = node.get("inputs", [])
                else:
                    need = ["input"]
            if all((dep in done) for dep in need):
                src = done[need[0]] if need else done["input"]
                op = node["op"]
                if op == "select" or op == "project":
                    cols = node["columns"]
                    got = []
                    for r in src:
                        nr = {}
                        for c in cols:
                            if c in r:
                                nr[c] = r[c]
                        got.append(nr)
                elif op == "filter":
                    got = []
                    for r in src:
                        if "value" in node and r.get(node["key"]) == node["value"]:
                            got.append(r)
                        elif "in" in node and r.get(node["key"]) in node["in"]:
                            got.append(r)
                        elif "predicate" in node:
                            pr = node["predicate"]
                            (pop, arg), = pr.items()
                            v = r.get(node["key"])
                            if v is None:
                                continue
                            hit = {
                                "lt": v < arg,
                                "le": v <= arg,
                                "gt": v > arg,
                                "ge": v >= arg,
                                "eq": v == arg,
                                "ne": v != arg,
                            }.get(pop)
                            if hit:
                                got.append(r)
                elif op == "rename":
                    m = node.get("mapping", {})
                    got = []
                    for r in src:
                        nr = {}
                        for k, v in r.items():
                            nr[m.get(k, k)] = v
                        got.append(nr)
                elif op == "sort":
                    kk = node.get("keys") or [node["key"]]
                    got = list(src)
                    # insertion sort on the composite key, keeping ties stable
                    for i in range(1, len(got)):
                        item = got[i]
                        j = i - 1
                        while j >= 0 and _ref_less(item, got[j], kk, node.get("descending", False)):
                            got[j + 1] = got[j]
                            j -= 1
                        got[j + 1] = item
                elif op == "limit":
                    got = list(src)[: int(node["count"])]
                elif op == "distinct":
                    got = []
                    seen = []
                    for r in src:
                        mark = json.dumps(r, sort_keys=True)
                        if mark not in seen:
                            seen.append(mark)
                            got.append(r)
                elif op == "concat":
                    got = []
                    for name in node["inputs"]:
                        got = got + list(done[name])
                elif op == "export":
                    got = list(src)
                else:
                    raise PlanError(f"reference: unsupported op {op!r}")
                done[node["id"]] = got
                progress = True
            else:
                still.append(node)
        remaining = still

    if remaining:
        raise PlanError("reference: unresolved nodes (cycle or missing input)")
    return done[plan[-1]["id"]] if plan else rows


def _ref_less(a, b, keys, descending):
    for k in keys:
        av = a.get(k)
        bv = b.get(k)
        if av == bv:
            continue
        if descending:
            return av > bv
        return av < bv
    # tie-break on canonical form for total determinism
    return json.dumps(a, sort_keys=True) < json.dumps(b, sort_keys=True)


def compare_results(primary, reference, *, context: str = "plan") -> None:
    """Raise if the engine and reference disagree on a result set."""
    if result_digest(primary) != result_digest(reference):
        raise AssertionError(
            f"engine/reference divergence for {context}: "
            f"engine={result_digest(primary)} reference={result_digest(reference)}"
        )


def plan_digest(plan) -> str:
    return result_digest(plan) if isinstance(plan, list) else result_digest([plan])


# --------------------------------------------------------------------------- #
# IO helpers that preserve the v1 public surface.
# --------------------------------------------------------------------------- #

def read_jsonl(path: str | Path):
    """Read a JSONL file with strict schema inference (v1-compatible surface)."""
    text = Path(path).read_text(encoding="utf-8")
    return parse_jsonl(text, strict=True)


def write_json(path, rows):
    payload = json.dumps(rows, ensure_ascii=False, sort_keys=True, indent=2) + "\n"
    Path(path).write_text(payload, encoding="utf-8")
