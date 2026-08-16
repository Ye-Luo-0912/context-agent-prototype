import pathlib, sys
root = pathlib.Path(sys.argv[1])
text = (root / "src" / "store.rs").read_text(encoding="utf-8")
sys.exit(0 if "anonymous" in text and "lookup(id)?" not in text else 1)
