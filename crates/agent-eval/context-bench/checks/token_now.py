import pathlib
import sys

root = pathlib.Path(sys.argv[1])
token = (root / "src" / "token.rs").read_text(encoding="utf-8")
auth = (root / "src" / "auth.rs").read_text(encoding="utf-8")
session = (root / "src" / "session.rs").read_text(encoding="utf-8")
ok = (
    "fn verify" in token
    and "fn parse" not in token
    and "now" in auth
    and "now" in session
    and "Token::verify" in auth
    and "Token::verify" in session
)
sys.exit(0 if ok else 1)
