"""TOOLS-09 Python lexical symbol rules (def / async def / class)."""

import re

_DEF = re.compile(r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)")
_CLASS = re.compile(r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)")


def symbols(source: str):
    """Return (kind, name, line) triples. Comment lines are skipped."""
    rows = []
    for line_no, line in enumerate(source.splitlines(), 1):
        if line.lstrip().startswith("#"):
            continue
        found = _DEF.match(line)
        if found:
            rows.append(("def", found.group(1), line_no))
            continue
        found = _CLASS.match(line)
        if found:
            rows.append(("class", found.group(1), line_no))
    return rows
