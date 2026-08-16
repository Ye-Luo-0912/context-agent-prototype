import pathlib
import sys

root = pathlib.Path(sys.argv[1])
text = (root / "src" / "config.rs").read_text(encoding="utf-8")
ok = (
    "APP_HOST" in text
    and "APP_PORT" in text
    and "env::var" in text
    and "prefer JSON" not in text
)
sys.exit(0 if ok else 1)
