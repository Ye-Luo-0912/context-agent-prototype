"""Pass the latest matching user-supplied trial key to stdin only.

The key is never written to a file, argument, environment, log or receipt by
this launcher. Supply the exact current Codex JSONL and a phrase identifying
the user's key message; no broad session search is performed.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess


def credential_from_user_message(session: Path, required_phrase: str) -> str:
    matches = []
    with session.open(encoding="utf-8") as stream:
        for line in stream:
            row = json.loads(line)
            payload = row.get("payload", {})
            if (row.get("type") != "response_item"
                    or payload.get("type") != "message"
                    or payload.get("role") != "user"):
                continue
            text = "\n".join(
                part.get("text", "") for part in payload.get("content", [])
                if isinstance(part, dict) and part.get("type") == "input_text"
            )
            if required_phrase not in text:
                continue
            keys = re.findall(r"(?<![A-Za-z0-9_-])sk-[A-Za-z0-9_-]{20,}(?![A-Za-z0-9_-])", text)
            matches.append(set(keys))
    if not matches:
        raise RuntimeError("expected exactly one matching user key message")
    unique_keys = matches[-1]
    if len(unique_keys) != 1:
        raise RuntimeError("latest identified user message needs exactly one unique trial key")
    return next(iter(unique_keys))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session-jsonl", type=Path, required=True)
    parser.add_argument("--required-phrase", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("a child command is required")
    key = credential_from_user_message(args.session_jsonl, args.required_phrase)
    child_env = {
        name: value for name, value in os.environ.items()
        if not name.startswith(("MIMO_", "XIAOMI_", "OPENAI_", "DEEPSEEK_"))
    }
    return subprocess.run(
        command, input=(key + "\n").encode(), env=child_env,
        check=False,
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
