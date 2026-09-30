"""Terminal-Bench pilot integration helpers.

The package deliberately has no Harbor dependency at import time.  The
preflight and command builder remain runnable on a developer machine that has
only the Python standard library; Harbor is imported lazily by the optional
custom-agent entry point.
"""
