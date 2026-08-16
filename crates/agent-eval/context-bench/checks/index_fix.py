import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
text = (root / "src" / "parse.rs").read_text(encoding="utf-8")
match = re.search(r"fn visit_all[\s\S]*?\n\}", text)
body = match.group(0) if match else text
off_by_one = "i + 1" in body or "i+1" in body
ok = "items" in body and not off_by_one
sys.exit(0 if ok else 1)
