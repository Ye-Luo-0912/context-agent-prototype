import pathlib
import sys

root = pathlib.Path(sys.argv[1])
text = (root / "src" / "store.rs").read_text(encoding="utf-8")
ok = (
    "anonymous" in text
    and "lookup(id)?" not in text
    and "cached_load" in text
    and "load_user" in text
)
sys.exit(0 if ok else 1)
