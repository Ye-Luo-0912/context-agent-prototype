"""Record only explicitly reviewed source paths; never scan user metadata."""
import hashlib
import json
import subprocess
from pathlib import Path

repo = Path(__file__).resolve().parents[4]
paths = [
    "crates/agent-runtime/src/actor/model.rs",
    "crates/agent-runtime/src/actor/turn.rs",
    "crates/agent-runtime/src/actor/lifecycle.rs",
    "crates/agent-runtime/src/actor/restore.rs",
    "crates/agent-runtime/src/directive.rs",
    "crates/agent-runtime/src/prompt.rs",
    "crates/agent-runtime/src/execution/memo.rs",
    "crates/agent-runtime/src/execution/body_cache.rs",
    "crates/agent-runtime/src/execution/state.rs",
    "crates/agent-contracts/src/model.rs",
    "crates/agent-contracts/src/context.rs",
    "crates/agent-contracts/src/discovery.rs",
    "crates/context-simple/src/engine.rs",
    "crates/context-simple/src/store.rs",
    "crates/context-simple/src/index/catalog.rs",
    "crates/context-baselines/src/rolling.rs",
    "crates/agent-compose/src/compactor.rs",
    "crates/tool-runtime/src/tools/artifact.rs",
    "crates/tool-runtime/src/tools/patch.rs",
    "crates/provider-openai/src/lib.rs",
    "crates/provider-openai/src/responses.rs",
    "crates/provider-openai/src/sse.rs",
    "crates/provider-openai/src/wire_names.rs",
]
rows = []
for name in paths:
    data = (repo / name).read_bytes()
    rows.append({"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=repo, text=True).strip()
result = {"review_start": "8a0dc29032a3175b203545addca7549efe90a495", "recorded_head": head,
          "scope": "Targeted W01-W08 and prompt/cache source review, not every line of the repository",
          "files": rows}
Path(__file__).with_name("source-manifest.json").write_text(
    json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(f"recorded {len(rows)} explicitly reviewed source files at {head}")
