import pathlib
import sys

root = pathlib.Path(sys.argv[1])
auth = (root / "src" / "auth.rs").read_text(encoding="utf-8")
billing = (root / "src" / "billing.rs").read_text(encoding="utf-8")
ok = (
    "operator" in auth
    and "rate_limit" in auth
    and "30" in auth
    and "i + 1" not in billing
    and "i+1" not in billing
)
sys.exit(0 if ok else 1)
