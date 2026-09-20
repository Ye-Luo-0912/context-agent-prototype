"""Frozen V5 workload: schedule, frozen capacity plan, and pacing.

Single source of truth shared by ``prepare.py`` (capacity plan), ``preflight.py``
(satisfiability gate) and ``continuous_load.py`` (execution), so the plan that
was proven satisfiable is the plan that actually runs.

The frozen workload is bounded on purpose: the descriptor member carries one
receipt row per distinct install, so the number of installs determines the
largest archive member. ``INSTALL_BUDGET`` is derived from the per-member byte
bound, and the writer rate is paced so that the frozen budget spans the frozen
duration instead of being consumed in the first minutes.
"""
from __future__ import annotations

from oracle import ARCHIVE_BYTE_BOUND, ARCHIVE_MEMBER_BOUND, MEMBER_BYTE_BOUND, TOTAL_UNCOMPRESSED_BOUND

CATALOG_PACKAGES = 120
CATALOG_VERSIONS = 4
PAYLOAD_LINES = 40
TENANT_COUNT = 8
ENVIRONMENT_COUNT = 2

# Frozen install budget. 4096 installs bound the descriptor member to about
# 1.6 MiB, inside the 2 MiB per-member bound, and bound the member count to
# about 626, inside the 1024-member bound. The budget is split across the frozen
# number of windows because a resumed window must continue with new identities
# instead of replaying old ones.
INSTALL_BUDGET_PER_WINDOW = 1024
MAX_WINDOWS = 4
INSTALL_BUDGET = INSTALL_BUDGET_PER_WINDOW * MAX_WINDOWS
BACKUP_EVERY = 4
WRITER_WORKERS = 4
CRASH_BOUNDARIES = 1
PUBLISH_ATTEMPTS = 4
PUBLISH_SLOT = 2
CRASH_SLOT = 5


def window_budget(window_index: int) -> int:
    """Absolute slot cap of one window; slot numbers never restart."""
    if not 1 <= window_index <= MAX_WINDOWS:
        raise ValueError(f"window index {window_index} outside the frozen {MAX_WINDOWS} windows")
    return min(window_index * INSTALL_BUDGET_PER_WINDOW, INSTALL_BUDGET)

# Analytic per-item bounds used by the capacity plan. Each one is a documented
# upper bound, not a measurement of one particular run.
RECEIPT_ROW_BOUND_BYTES = 384
JOURNAL_ROW_BOUND_BYTES = 512
OUTBOX_ROW_BOUND_BYTES = 512
POINTER_ROW_BOUND_BYTES = 256
MANIFEST_BOUND_BYTES = 1024
DESCRIPTOR_OVERHEAD_BYTES = 4096
ZIP_OVERHEAD_BYTES = 1024
ZIP_OVERHEAD_RATIO = 1.02

SEED = dict(tenant="tenant-00", environment="staging", key="seed",
            requirements={"pkg-000": {"min": 1, "max": 2}})


def scope_for(slot: int) -> tuple:
    return (f"tenant-{slot % TENANT_COUNT:02d}", "staging" if slot % 3 == 0 else "prod")


def requirements_for(slot: int) -> dict:
    return {f"pkg-{slot % CATALOG_PACKAGES:03d}": {"min": 1 + slot % 3, "max": 2 + slot % 3}}


def key_for(slot: int) -> str:
    return f"release-{slot:06d}"


def request(slot: int, generation: int | None) -> dict:
    tenant, environment = scope_for(slot)
    return dict(tenant=tenant, environment=environment, key=key_for(slot),
                requirements=requirements_for(slot), expected_generation=generation)


def plan_slot(slot: int) -> dict:
    """What a slot performs. Read/GC work and backups are controller-owned."""
    return dict(slot=slot, install=True, active=slot % 2 == 0,
                backup=slot % BACKUP_EVERY == 0,
                publish=slot == PUBLISH_SLOT, crash=slot == CRASH_SLOT)


def payload_bytes(name: str, version: int) -> bytes:
    return (f"package={name};version={version};\n" + "data-package-line\n" * PAYLOAD_LINES).encode("utf-8")


def payload_bound_bytes() -> int:
    """Largest catalog payload; every object is content-addressed by it."""
    return max(len(payload_bytes(f"pkg-{index:03d}", version))
               for index in range(CATALOG_PACKAGES) for version in range(1, CATALOG_VERSIONS + 1))


def closure_plan(*, install_budget: int = INSTALL_BUDGET, writer_workers: int = WRITER_WORKERS,
                 crash_boundaries: int = CRASH_BOUNDARIES, publish_attempts: int = PUBLISH_ATTEMPTS) -> dict:
    """Maximum referenced closure of the frozen workload.

    Every number is derived from the frozen schedule, so a workload change that
    would not fit the frozen archive bounds is caught before a window opens.
    """
    scopes = {SEED["tenant"] + "/" + SEED["environment"]}
    manifests = {("seed", SEED["tenant"], SEED["environment"], "pkg-000", 1)}
    for slot in range(1, install_budget + 1):
        tenant, environment = scope_for(slot)
        scopes.add(tenant + "/" + environment)
        package = f"pkg-{slot % CATALOG_PACKAGES:03d}"
        minimum = 1 + slot % 3
        manifests.add((slot % 24, tenant, environment, package, minimum))
    # Distinct manifest content is (tenant, environment, resolved package set);
    # dependencies couple consecutive packages, so every distinct (scope,
    # package, minimum) triple is counted separately and one manifest may serve
    # several receipts.
    manifest_count = len({(tenant, environment, package, minimum)
                          for _, tenant, environment, package, minimum in manifests})
    object_count = CATALOG_PACKAGES * CATALOG_VERSIONS
    scope_count = len(scopes)
    journal_count = writer_workers + crash_boundaries
    outbox_count = publish_attempts
    member_count = 1 + manifest_count + object_count + scope_count + journal_count + outbox_count
    descriptor_bytes = (DESCRIPTOR_OVERHEAD_BYTES
                        + install_budget * RECEIPT_ROW_BOUND_BYTES
                        + journal_count * JOURNAL_ROW_BOUND_BYTES
                        + outbox_count * OUTBOX_ROW_BOUND_BYTES)
    manifest_bytes = manifest_count * MANIFEST_BOUND_BYTES
    object_bytes = object_count * payload_bound_bytes()
    pointer_bytes = scope_count * POINTER_ROW_BOUND_BYTES
    uncompressed_bytes = descriptor_bytes + manifest_bytes + object_bytes + pointer_bytes
    archive_bytes = int(uncompressed_bytes * ZIP_OVERHEAD_RATIO) + ZIP_OVERHEAD_BYTES
    plan = dict(
        install_budget=install_budget,
        writer_workers=writer_workers,
        receipts=install_budget,
        scopes=scope_count,
        manifests=manifest_count,
        objects=object_count,
        journal_rows=journal_count,
        outbox_rows=outbox_count,
        members=member_count,
        descriptor_member_bytes=descriptor_bytes,
        uncompressed_bytes=uncompressed_bytes,
        archive_bytes=archive_bytes,
        bounds=dict(members=ARCHIVE_MEMBER_BOUND,
                    member_bytes=MEMBER_BYTE_BOUND,
                    uncompressed_bytes=TOTAL_UNCOMPRESSED_BOUND,
                    archive_bytes=ARCHIVE_BYTE_BOUND),
        derivations=dict(
            receipts="one receipt row per frozen install",
            manifests="distinct (tenant, environment, package, minimum) triples in the frozen schedule",
            objects="catalog packages x versions, content-addressed",
            journal_rows="writer workers + crash boundaries",
            outbox_rows="planned publish attempts",
            descriptor_member="receipt + journal + outbox rows and pointers",
        ),
    )
    plan["exceeds"] = sorted(name for name, value in (
        ("members", member_count), ("member_bytes", descriptor_bytes),
        ("uncompressed_bytes", uncompressed_bytes), ("archive_bytes", archive_bytes),
    ) if value > plan["bounds"][name])
    plan["satisfiable"] = not plan["exceeds"]
    return plan


def assert_satisfiable(plan: dict) -> None:
    if not plan.get("satisfiable"):
        raise ValueError(
            "frozen workload exceeds the frozen archive bounds: "
            + ", ".join(f"{name}={plan['bounds'][name]}" for name in plan["exceeds"]))


SPEC_BOUND_LABELS = (
    ("member count", "members", False),
    ("single member bytes", "member_bytes", True),
    ("uncompressed total", "uncompressed_bytes", True),
    ("archive bytes", "archive_bytes", True),
)


def check_spec_bounds(text: str) -> dict:
    """Compare the numbers stated in the frozen contract with the constants the
    oracle actually enforces. A mismatch means the contract and the judge have
    drifted apart, which is exactly what the review found (F01)."""
    import re
    parsed = {}
    for label, key, in_mib in SPEC_BOUND_LABELS:
        match = re.search(rf"^{re.escape(label)}\s+([0-9]+)(\s*MiB)?\s*$", text, re.MULTILINE)
        if match is None:
            raise ValueError(f"frozen contract does not state {label!r}")
        value = int(match.group(1))
        parsed[key] = value * 1024 * 1024 if in_mib else value
    expected = dict(members=ARCHIVE_MEMBER_BOUND, member_bytes=MEMBER_BYTE_BOUND,
                    uncompressed_bytes=TOTAL_UNCOMPRESSED_BOUND, archive_bytes=ARCHIVE_BYTE_BOUND)
    mismatches = sorted(key for key in expected if parsed[key] != expected[key])
    return dict(parsed=parsed, expected=expected, mismatches=mismatches, agrees=not mismatches)
