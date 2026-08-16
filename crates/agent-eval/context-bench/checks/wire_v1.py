import pathlib, sys
root = pathlib.Path(sys.argv[1])
text = (root / "src" / "protocol.rs").read_text(encoding="utf-8")
ok = ("decode_v1" in text or "ping" in text) and ("v\":2" in text or "Hello" in text)
sys.exit(0 if ok else 1)
