"""Official SWE-bench gold eval launcher.

On Windows, `swebench.harness` imports Unix-only `resource` via package init.
`fsspec` must see that import fail first, or it takes the Unix branch. Load
`datasets` before installing a tiny stub, then hand off to the harness.

Also force LF when the harness writes `eval.sh` — CRLF inside the Linux
container makes `pipefail\\r` / `pytest\\r` fail.
"""

from __future__ import annotations

import pathlib
import runpy
import sys
import types


def _windows_resource_stub() -> None:
    if sys.platform != "win32":
        return
    import datasets  # noqa: F401

    if "resource" in sys.modules:
        return
    mod = types.ModuleType("resource")
    mod.RLIMIT_NOFILE = 7
    mod.getrlimit = lambda *_a, **_k: (1024, 1024)
    mod.setrlimit = lambda *_a, **_k: None
    mod.error = OSError
    sys.modules["resource"] = mod


def _force_lf_write_text() -> None:
    original = pathlib.Path.write_text

    def write_text(self, data, encoding=None, errors=None, newline="\n"):  # type: ignore[no-untyped-def]
        if isinstance(data, str):
            data = data.replace("\r\n", "\n").replace("\r", "\n")
        return original(self, data, encoding=encoding, errors=errors, newline=newline)

    pathlib.Path.write_text = write_text  # type: ignore[method-assign]


if __name__ == "__main__":
    _windows_resource_stub()
    _force_lf_write_text()
    sys.argv[0] = "swebench.harness.run_evaluation"
    runpy.run_module("swebench.harness.run_evaluation", run_name="__main__")
