"""Verify switch-resume: operator allow, billing index fix, and rate_limit API.

`pub fn rate_limit(...) -> u32` must compile and return 30. A comment or
identifier substring is not enough.
"""

from __future__ import annotations

import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

root = pathlib.Path(sys.argv[1])
auth_path = root / "src" / "auth.rs"
billing = (root / "src" / "billing.rs").read_text(encoding="utf-8")
auth_text = auth_path.read_text(encoding="utf-8")

if "operator" not in auth_text:
    sys.stderr.write("allow() must still accept operator\n")
    sys.exit(1)
if "i + 1" in billing or "i+1" in billing:
    sys.stderr.write("billing still uses items[i + 1]\n")
    sys.exit(1)


def rustc_cmd() -> list[str]:
    rustc = shutil.which("rustc")
    if rustc:
        return [rustc]
    rustup = shutil.which("rustup")
    if rustup:
        toolchain = (
            "stable-x86_64-pc-windows-msvc" if os.name == "nt" else "stable"
        )
        return [rustup, "run", toolchain, "rustc"]
    sys.stderr.write("rustc not found\n")
    sys.exit(1)


def run(argv: list[str], cwd: pathlib.Path) -> None:
    result = subprocess.run(
        argv,
        cwd=cwd,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr or result.stdout or "command failed\n")
        sys.exit(1)


rustc = rustc_cmd()
workdir = pathlib.Path(tempfile.mkdtemp(prefix="switch_resume_"))
try:
    lib_src = workdir / "auth.rs"
    shutil.copyfile(auth_path, lib_src)
    rlib = workdir / "libauth.rlib"
    run(
        rustc
        + [
            "--edition",
            "2021",
            "--crate-type",
            "rlib",
            "--crate-name",
            "auth",
            str(lib_src),
            "-o",
            str(rlib),
        ],
        workdir,
    )
    driver = workdir / "driver.rs"
    driver.write_text(
        "fn main() {\n"
        "    assert_eq!(auth::rate_limit(\"user\"), 30);\n"
        "}\n",
        encoding="utf-8",
    )
    exe = workdir / ("driver.exe" if os.name == "nt" else "driver")
    run(
        rustc
        + [
            "--edition",
            "2021",
            str(driver),
            "--extern",
            f"auth={rlib}",
            "-o",
            str(exe),
        ],
        workdir,
    )
    run([str(exe)], workdir)
finally:
    shutil.rmtree(workdir, ignore_errors=True)
