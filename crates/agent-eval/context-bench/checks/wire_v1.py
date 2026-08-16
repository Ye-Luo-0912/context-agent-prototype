import pathlib
import sys

root = pathlib.Path(sys.argv[1])
text = (root / "src" / "protocol.rs").read_text(encoding="utf-8")
has_hello = "Hello" in text
has_v2 = '"v":2' in text or "v:2" in text or r'v\":2' in text
has_ping_decode = "fn decode" in text and "ping" in text
sys.exit(0 if has_hello and has_v2 and has_ping_decode else 1)
