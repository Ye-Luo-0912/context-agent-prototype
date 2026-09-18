"""Strict schema validation for CSV and JSONL ingest.

Design goals
------------
* Validation is *strict*: unknown/missing fields, wrong types, malformed rows
  and duplicate primary keys are rejected with a precise, line/row addressed
  error. Callers may opt into a lenient policy, but the default the platform
  uses for durable intake is strict.
* Validation is *pure* and deterministic: it never touches the network, never
  executes payloads and depends only on its inputs. This makes it safe to run
  inside worker processes and reusable by the independent reference path.
* The canonical schema below is small on purpose (id, kind, amount) because it
  matches the v1 fixture, but the machinery generalises to arbitrary typed
  columns.
"""

from __future__ import annotations

import csv
import io
import json
import re
from dataclasses import dataclass, field
from typing import Any, Dict, List, Sequence

__all__ = [
    "SchemaError",
    "Field",
    "Schema",
    "DEFAULT_SCHEMA",
    "validate_rows",
    "parse_jsonl",
    "parse_csv",
    "parse_records",
]

_UNSET = object()

# A JSON number that is large loses precision as a float; we keep integers as
# ints and reject anything that is not finite.
_NUMBER_RE = re.compile(r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?$")


class SchemaError(ValueError):
    """Raised when input data violates the declared schema."""

    def __init__(self, message: str, *, location: str | None = None, row_index: int | None = None):
        self.location = location
        self.row_index = row_index
        prefix = ""
        if location:
            prefix = f"[{location}] "
        elif row_index is not None:
            prefix = f"[row {row_index}] "
        super().__init__(prefix + message)


@dataclass(frozen=True)
class Field:
    name: str
    type: str = "string"  # one of: string, int, float, bool
    required: bool = True
    enum: tuple | None = None
    minimum: float | None = None
    maximum: float | None = None
    coerce: bool = False

    def _coerce(self, value: Any, *, location: str) -> Any:
        t = self.type
        if t == "string":
            if not isinstance(value, str):
                raise SchemaError(f"field {self.name!r} expected string", location=location)
            return value
        if t == "int":
            if isinstance(value, bool):
                raise SchemaError(f"field {self.name!r} expected int", location=location)
            if isinstance(value, int):
                return value
            if self.coerce and isinstance(value, str) and re.fullmatch(r"-?[0-9]+", value.strip()):
                return int(value.strip())
            raise SchemaError(f"field {self.name!r} expected int", location=location)
        if t == "float":
            if isinstance(value, bool):
                raise SchemaError(f"field {self.name!r} expected float", location=location)
            if isinstance(value, (int, float)):
                return float(value)
            if self.coerce and isinstance(value, str) and _NUMBER_RE.fullmatch(value.strip()):
                return float(value.strip())
            raise SchemaError(f"field {self.name!r} expected float", location=location)
        if t == "bool":
            if isinstance(value, bool):
                return value
            if self.coerce and isinstance(value, str) and value.strip().lower() in {"true", "false"}:
                return value.strip().lower() == "true"
            raise SchemaError(f"field {self.name!r} expected bool", location=location)
        raise SchemaError(f"unknown field type {t!r}", location=location)

    def check(self, value: Any, *, location: str) -> Any:
        if value is _UNSET:
            if self.required:
                raise SchemaError(f"missing required field {self.name!r}", location=location)
            return _UNSET
        value = self._coerce(value, location=location)
        if self.enum is not None and value not in self.enum:
            raise SchemaError(
                f"field {self.name!r} value {value!r} not in {list(self.enum)!r}",
                location=location,
            )
        if self.minimum is not None and value < self.minimum:
            raise SchemaError(f"field {self.name!r} below minimum {self.minimum}", location=location)
        if self.maximum is not None and value > self.maximum:
            raise SchemaError(f"field {self.name!r} above maximum {self.maximum}", location=location)
        return value


@dataclass
class Schema:
    fields: List[Field]
    primary_key: str | None = None
    allow_unknown: bool = False

    def _field_map(self) -> Dict[str, Field]:
        return {f.name: f for f in self.fields}

    def validate(self, row: Any, *, location: str) -> Dict[str, Any]:
        if not isinstance(row, dict):
            raise SchemaError("row must be a JSON object", location=location)
        fmap = self._field_map()
        if not self.allow_unknown:
            unknown = set(row) - set(fmap)
            if unknown:
                raise SchemaError(
                    f"unknown field(s): {sorted(unknown)!r}", location=location
                )
        out: Dict[str, Any] = {}
        for f in self.fields:
            val = f.check(row.get(f.name, _UNSET), location=location)
            if val is not _UNSET:
                out[f.name] = val
        return out


# Canonical schema used by the platform for the fixture-shaped records.
DEFAULT_SCHEMA = Schema(
    fields=[
        Field("id", "int", required=True),
        Field("kind", "string", required=True),
        Field("amount", "int", required=True),
    ],
    primary_key="id",
)


def _detect_schema(row: dict) -> Schema:
    """Infer a strict schema from a sample row's field types."""
    fields = []
    for name, value in row.items():
        if isinstance(value, bool):
            t = "bool"
        elif isinstance(value, int):
            t = "int"
        elif isinstance(value, float):
            t = "float"
        elif isinstance(value, str):
            t = "string"
        else:
            raise SchemaError(f"cannot infer schema for field {name!r}")
        fields.append(Field(name, t, required=True))
    return Schema(fields=fields, primary_key="id" if "id" in row else None)


def parse_jsonl(text: str, *, schema: Schema | None = None, strict: bool = True) -> List[Dict[str, Any]]:
    """Parse a JSONL document into validated rows.

    ``strict`` controls how the absence of an explicit schema is treated: when
    True the schema is inferred from the first record and then enforced for
    every subsequent record. Line numbers are 1-based and reported in errors.
    """
    raw: List[dict] = []
    for lineno, line in enumerate(text.splitlines(), start=1):
        if not line.strip():
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError as exc:
            raise SchemaError(f"invalid JSON: {exc.msg}", location=f"line {lineno}") from exc
        if isinstance(obj, list):
            raise SchemaError("each JSONL line must be a single object", location=f"line {lineno}")
        raw.append(obj)
    if schema is None:
        if not raw:
            return []
        schema = _detect_schema(raw[0]) if strict else None
    return validate_rows(raw, schema=schema) if schema is not None else raw


def parse_csv(
    text: str,
    *,
    schema: Schema | None = None,
    strict: bool = True,
    delimiter: str = ",",
) -> List[Dict[str, Any]]:
    """Parse a CSV document into validated rows with typed coercion.

    Strict mode requires the header to exactly match the schema field names and
    rejects ragged rows. Values are coerced to the declared field types.
    """
    reader = csv.reader(io.StringIO(text), delimiter=delimiter)
    rows = list(reader)
    rows = [r for r in rows if any(cell.strip() for cell in r)]
    if not rows:
        return []
    header = [h.strip() for h in rows[0]]
    if len(set(header)) != len(header):
        raise SchemaError("duplicate column names in CSV header", location="header")
    records = []
    for i, cells in enumerate(rows[1:], start=1):
        if len(cells) != len(header):
            raise SchemaError(
                f"expected {len(header)} columns, found {len(cells)}",
                location=f"row {i}",
            )
        records.append(dict(zip(header, cells)))
    if schema is None:
        if not strict:
            return records
        # Infer from the raw string values of the first record.
        schema = _detect_schema(records[0])
        return validate_rows(records, schema=schema, coerce=True)
    return validate_rows(records, schema=schema, coerce=True)


def parse_records(
    raw: Sequence[Any],
    *,
    schema: Schema,
    primary_key: str | None = None,
    coerce: bool = False,
) -> List[Dict[str, Any]]:
    """Validate already-decoded records (in-memory) against a schema."""
    return validate_rows(raw, schema=schema, primary_key=primary_key, coerce=coerce)


def validate_rows(
    rows: Sequence[Any],
    *,
    schema: Schema,
    primary_key: str | None = None,
    coerce: bool = False,
) -> List[Dict[str, Any]]:
    """Validate rows, optionally coercing types and enforcing primary keys."""
    effective_schema = schema
    if coerce or primary_key is not None:
        effective_schema = Schema(
            fields=[Field(f.name, f.type, f.required, f.enum, f.minimum, f.maximum, coerce) for f in schema.fields],
            primary_key=primary_key if primary_key is not None else schema.primary_key,
            allow_unknown=schema.allow_unknown,
        )
    pk = effective_schema.primary_key
    seen: Dict[Any, int] = {}
    out: List[Dict[str, Any]] = []
    for i, row in enumerate(rows):
        validated = effective_schema.validate(row, location=f"row {i}")
        if pk is not None and pk in validated:
            key = validated[pk]
            if key in seen:
                raise SchemaError(
                    f"duplicate primary key {pk}={key!r} (first at row {seen[key]})",
                    row_index=i,
                )
            seen[key] = i
        out.append(validated)
    return out
