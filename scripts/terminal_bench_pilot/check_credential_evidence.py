"""Check new local evidence without printing or copying the provider credential."""
from pathlib import Path
import json
import sys
from runtime_endurance_incremental_runner import build_env


def contains(stream, needle: bytes) -> bool:
    tail = b""
    while chunk := stream.read(1024 * 1024):
        block = tail + chunk
        if needle in block:
            return True
        tail = block[-max(1, len(needle) - 1):]
    return False


def main():
    repo = Path(__file__).resolve().parents[2]
    needle = build_env(repo)["OPENAI_API_KEY"].encode()
    if len(needle) < 16:
        raise ValueError("provider credential has unexpected shape; scan refused")
    checked, affected = 0, []
    for root in map(Path, sys.argv[1:]):
        if not root.is_dir():
            raise ValueError("evidence directory missing")
        for path in root.rglob("*"):
            if path.is_file() and not path.is_symlink():
                with path.open("rb") as stream:
                    found = contains(stream, needle)
                checked += 1
                if found:
                    affected.append(str(path))
    if not checked:
        raise ValueError("no evidence scanned")
    print(json.dumps({"files_scanned": checked,
                      "account_credential_present": bool(affected),
                      "affected_files": affected}))
    return int(bool(affected))


if __name__ == "__main__":
    raise SystemExit(main())
