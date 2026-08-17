"""Verify switch-resume by compile-and-call, not file substrings.

`allow("operator")` must be true, `total([1, 2, 3])` must be 6, and
`rate_limit("user")` must return 30. Comments, dead code, or a leftover
identifier cannot pass.
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
billing_path = root / "src" / "billing.rs"


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


def compile_rlib(rustc: list[str], src: pathlib.Path, name: str, dest: pathlib.Path) -> pathlib.Path:
    rlib = dest / f"lib{name}.rlib"
    run(
        rustc
        + [
            "--edition",
            "2021",
            "--crate-type",
            "rlib",
            "--crate-name",
            name,
            str(src),
            "-o",
            str(rlib),
        ],
        dest,
    )
    return rlib


rustc = rustc_cmd()
workdir = pathlib.Path(tempfile.mkdtemp(prefix="switch_resume_"))
try:
    auth_rlib = compile_rlib(rustc, auth_path, "auth", workdir)
    billing_rlib = compile_rlib(rustc, billing_path, "billing", workdir)
    driver = workdir / "driver.rs"
    driver.write_text(
        "fn main() {\n"
        "    assert!(auth::allow(\"admin\"));\n"
        "    assert!(auth::allow(\"operator\"));\n"
        "    assert!(!auth::allow(\"nobody\"));\n"
        "    assert_eq!(auth::rate_limit(\"user\"), 30);\n"
        "    assert_eq!(billing::total(&[1, 2, 3]), 6);\n"
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
            f"auth={auth_rlib}",
            "--extern",
            f"billing={billing_rlib}",
            "-o",
            str(exe),
        ],
        workdir,
    )
    run([str(exe)], workdir)
finally:
    shutil.rmtree(workdir, ignore_errors=True)
