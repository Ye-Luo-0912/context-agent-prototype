# M15 acceptance design (V1) — frozen draft, pending operator sign-off

This document is the "separately frozen acceptance design" that ROADMAP and
[`LONG_TASK_EVALUATION.md`](LONG_TASK_EVALUATION.md) require before formal
M15. It pins what V1 is, which evidence planes count, how cells are judged,
and what may not change mid-run. The four decision points at the end need
explicit operator sign-off before the first cell runs; everything else here
is binding once that sign-off lands.

## 1. What V1 is — the candidate composition

V1 is the composition at the current HEAD, backed by these banked planes:

| plane | evidence (banked, frozen) | status |
| --- | --- | --- |
| Platform gates | M12/M13 closure-audit reports, clean-tree PASS (`evidence/platform-closure/{m12,m13}/`) | closed 2026-08-27 |
| Context | `context-mech.v2` 12-cell A/C live (`evidence/context-mech/`); frozen SPEC; GC/context policy untouched since | banked; **no rerun** |
| Tool Surface | edit-gate `v4` archival 4×3 window (strict 12/12, gate 12/12, zero confirm reads) + deterministic crash/race/journal/disk-full suite | banked |
| Execution coherence | Convergence Bench 4/4 deterministic on the acceptance HEAD (re-certified 2026-08-28); obligation-ledger and hidden-green live A/C longflow evidence (`longflow-post-obligation-2026-08-23/`) | banked + re-certified |
| Long-task truth chain | LT-RUN-05 WP1–WP5 deterministic matrix (snapshot fence, unified capture, two-phase completion, verification basis, tuple-only cold resume) | landed |
| Advisory switches | CompletionOpportunity ENDED default-off by its 2026-08-28 decision-grade gate; no candidate switches may be on | final |

V1 claims nothing about general task-failure rates: every plane's evidence is
a finite diagnostic over its frozen pack.

## 2. Planes and the live formal run (layered per EVAL-02)

The live M15 run covers exactly one plane — the **development plane**. The
other planes cite their banked evidence; none is rerun, and no plane borrows
another's cells.

Development pack (three tasks, per the layer-6 expansion rule):

1. `retry_policy_dev` — already frozen (fixture, directive, hidden oracle).
2. `retry_diag_dev` — one diagnosis task (to be authored): a small
   network-free crate with a seeded, reproducible defect; the hidden oracle
   asserts the diagnosis report names the defect mechanism and the minimal
   fix holds the suite.
3. `retry_migrate_dev` — one multi-file migration task (to be authored):
   rename/split across a bounded file set with a harness-owned API-compat
   oracle.

Modes: `normal` and `resume` (semantic interruption trigger: first durably
settled mutation + its durable checkpoint). Repeats: **2 per (task, mode)**
— 12 development cells total. All cells run on the same pinned serving
within one window; a mid-window provider loss voids the window (evidence
stays; the window reruns whole).

## 3. Per-cell artifacts (EVAL-01 rebuildability)

Every cell writes the versioned bundle (schema `retry-pilot-cell-v2` or its
successor with a bumped version): manifest (identity tuple incl.
`source_tree_digest`, model, pack digest), full event stream, per-dimension
`dimensions.json`, hidden-verification records, workspace snapshot hash.
Bundles are immutable once claimed; a harness failure is an explicit
NOT_RUN and cannot improve any verdict.

Pre-run work item: tasks 2 and 3 must ship their harness-owned,
network-free oracle crates and hidden checks **before** the window; authoring
them after the first cell is a freeze violation.

## 4. Pass criteria

Cell PASS = behavioral oracle PASS ∧ allowed-diff PASS ∧ provider/runtime
healthy ∧ no runtime error ∧ (resume mode only: restored ∧ continued on the
exact acknowledged tuple).

**Closure (`task.complete` lifecycle) is reported per cell as a product fact
— completed | active | failed(reason) — but is not a mandatory pass
dimension for V1.** Grounding: the 2026-08-28 decision-grade window showed
model-autonomous closure is variance (the off baseline self-closed one cell;
the affordance candidate that tried to earn closure ended by its own gate).
Making it mandatory now would bind M15 to a product behavior the project
just declined to buy. The operator may raise it later for a V2 design.

Plane PASS = every cell PASS. M15 closure = all planes PASS + bundles
committed + REPORT mechanically derived from per-cell facts + zero
unresolved items on the acceptance path.

## 5. Freeze rules during the window

Pinned for the whole window: tool surface `v4`; LT-RUN substrate at the
acceptance HEAD; opportunity OFF; the C context composition; the host policy
snapshot; model/provider serving; pack contents; oracle sources; repeat
count. No source change lands between the first and last cell — a mid-window
fix voids the window. Reports are derived mechanically from bundles; hand
editing numbers is out of contract.

## 6. Explicitly parked

300×3 scale, `recall_after_fix`, 27-cell context expansion, a
second-context-engine A/C comparison (gated on a promoted frozen setting —
none exists), and the model-comparison layer (only after V1 closes).

## 7. Decision points requiring operator sign-off before the first cell

1. **Serving pin**: stay on the current `gpt-5.6-luna` @ PinAI serving, or
   wait for a materially different one (the 08-28 window saw three
   stream stalls in eight cells).
2. **Cost**: 12 development cells ≈ the 08-28 window's order of magnitude
   per cell; confirm the budget.
3. **Fixture authoring**: approve the `retry_diag_dev` / `retry_migrate_dev`
   specs (or substitute), since their oracles must exist and freeze first.
4. **Closure reporting**: confirm closure stays a reported (non-mandatory)
   dimension for V1.
