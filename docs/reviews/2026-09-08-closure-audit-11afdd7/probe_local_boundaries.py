"""Isolated OS/mechanical probes, NOT repository Rust/.NET tests."""
import hashlib, json, os, pathlib, socket, struct, tempfile
out = pathlib.Path(__file__).parent
result = {"scope": "OS/mechanical probes only; no Rust/.NET repository code executed"}
with tempfile.TemporaryDirectory(prefix="agent-review-") as d:
    p = pathlib.Path(d) / "owned.sock"
    sockets = []
    try:
        old = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); sockets.append(old)
        old.settimeout(1); old.bind(str(p)); old.listen(2)
        before = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); sockets.append(before)
        before.settimeout(1); before.connect(str(p))
        old_peer, _ = old.accept(); sockets.append(old_peer); old_peer.settimeout(1)
        os.unlink(p)
        new = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); sockets.append(new)
        new.settimeout(1); new.bind(str(p)); new.listen(2)
        after = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); sockets.append(after)
        after.settimeout(1); after.connect(str(p))
        new_peer, _ = new.accept(); sockets.append(new_peer); new_peer.settimeout(1)
        before.sendall(b"old"); after.sendall(b"new")
        result["uds_unlink_active_listener"] = {
            "old_connection_still_operational": old_peer.recv(3) == b"old",
            "new_connections_reach_second_listener": new_peer.recv(3) == b"new",
            "production_host_executed": False,
        }
    finally:
        for s in reversed(sockets):
            s.close()
    p.unlink(missing_ok=True)
    # The exact unlink primitive also removes a regular file. All objects
    # are created by this probe in its private temporary directory.
    p.write_text("probe-owned content", encoding="utf-8")
    os.unlink(p)
    result["unlink_regular_file"] = {"probe_owned_file_removed": not p.exists()}
# Validate the file-opening mechanism using only our own temporary files.
with tempfile.TemporaryDirectory(prefix="agent-skill-review-") as d:
    base = pathlib.Path(d)
    package = base / "package"
    package.mkdir()
    external = base / "probe-owned-outside.txt"
    external.write_text("probe-owned body", encoding="utf-8")
    reference = pathlib.Path("steps.md")
    (package / reference).symlink_to(external)
    result["skill_relative_symlink_probe"] = {
        "reference": str(reference),
        "lexically_relative": not reference.is_absolute() and ".." not in reference.parts,
        "resolved_outside_package": not (package / reference).resolve().is_relative_to(package.resolve()),
        "normal_file_open_read_outside_package": (package / reference).read_text(encoding="utf-8") == "probe-owned body",
        "production_plugin_executed": False,
        "note": "OS mechanism only; all content was probe-owned; temporary directory is removed.",
    }
header = bytes([0, 0, 0, 9])
announced = struct.unpack("<I", header)[0]
result["csharp_half_frame_fixture"] = {
    "header_hex": header.hex(), "little_endian_length": announced,
    "frame_cap": 1024 * 1024,
    "actually_hits_oversize_branch": announced > 1024 * 1024,
    "correct_header_for_nine_bytes": struct.pack("<I", 9).hex(),
}
seed = b"focus-agent.platform.work.v1|run-scoped"
expected = "79eda3b0421ca507b2d9eaca68471dcaba350e3a971eda26efaeb691d60678cf"
result["schema_seed"] = {
    "matches_current_csharp_constant": hashlib.sha256(seed).hexdigest() == expected,
    "note": "matching constants are not an end-to-end contract proof",
}
result["cleanup"] = "All sockets closed; temporary directory removed. No repository/user files modified."
(out / "LOCAL_PROBES.json").write_text(json.dumps(result, ensure_ascii=False, indent=2)+"\n", encoding="utf-8")
print(json.dumps(result, ensure_ascii=False, indent=2))
