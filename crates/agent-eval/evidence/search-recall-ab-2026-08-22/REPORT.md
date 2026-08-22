# Search recall A/B — candidate union + token-coverage verification

Status: dirty-tree engine-only diagnostic. Not the paired real-model
coding gate; no provider was called. Both sides ran identical harness
code; only the search implementation differed between the two trees.

## Method

- Baseline tree: `git worktree` pinned at HEAD `0b1b999`
  (pre-change candidate logic: whole-needle key substring + residual scan,
  whole-needle-only verification).
- Current tree: working tree with the shared inverted-index kernel
  (`agent-contracts::search`) unioned into `ContextCatalog::search_ids`,
  plus a token-coverage verification fallback in `store.rs::search_entries`
  (whole-needle substring stays primary and ranking-privileged).
- Release builds on both sides; deterministic double-run asserts green on
  both sides; control-group asserts green on both sides.
- Scope: non-file retrieval surface (Decision/Note summaries, entities,
  stored descriptors). File identity paths untouched.

## Measurement 1 — store-level micro A/B (`store.rs::search_ab::ab_report`)

Corpus 411 resident + 102 stored items; limit=50; latency = mean over
200 rounds per case.

| Category | hit@1 old | hit@1 new | steady µs/query old → new |
| --- | --- | --- | --- |
| multi_word resident (4) | 0/4 | 4/4 | ~367 → ~2.5 |
| stored_multi_word (2) | 0/2 | 2/2 | ~348 → ~1.8 |
| legacy_exact entity (2) | 2/2 rank 1 | 2/2 rank 1 | 1.3 → 1.8 |
| legacy_substring fragment (1) | 1/1 rank 1 | 1/1 rank 1 | 1.2 → 2.5 |
| control absent terms | empty | empty | no false positives |

Catalog build+rebuild (one-time): 768 µs → 1739 µs for 513 items
(inverted-index writes), still sub-2 ms and amortized.

Old-side multi-word queries paid a full residual scan (~500 projections)
and still returned empty; new side answers from the index.

## Measurement 2 — real call chain (`agent-eval --retrieval-complex`)

Path: `ingest → maintain(AfterModel) → gc() externalize → search_external`
on `SimpleContextEngine`. GC behavior identical on both sides
(forgotten=5); the delta is purely the search layer. Mixed residency:
multi-word hits include items recovered from the externalized store
(`recovered_from_gc`).

| Category | hit cases old | hit cases new | first rank new |
| --- | --- | --- | --- |
| multi_word semantic (5) | 0/5 | 5/5 | all rank 1 |
| kind-filtered multi-word (1) | 0/1 | 1/1 | rank 1 |
| single-word controls (2) | 2/2 | 2/2 | rank 1 |
| identity path controls (2) | 2/2 | 2/2 | rank 1 |
| absent-term control | empty | empty | — |

Steady multi-word latency ~20 µs → ~4.5 µs. First query on each side
carries a one-time warm-up (79 µs old / 176 µs new).

## What changed to make this land

The candidate-layer recall did not reach callers until
`search_entries` verification stopped rejecting multi-word candidates:
a needle now also verifies when *every* token (shared `tokenize` rule,
≥2 chars) appears in one entry's matchable text (entities + file path +
summary + uri). AND-semantics keeps precision: tokens spread across
different entries never merge-match (asserted by unit tests). Contract
docs updated: `agent-contracts/src/context.rs` (`ContextSearchQuery::query`)
and `docs/CONTEXT_LIFECYCLE.md` §9g.

## Measurement 3 — live A/C mechanism cell (`--context-mech-run late_semantic_constraint`, repeats=1)

Real model (eval.env gpt-5.6-luna), paired independent workspaces,
hidden verification `file_content+command` 2/2 asserts PASSED on every
cell. Evidence bundles: `evidence/search-ab-context-mech-2026-08-22/`
(current tree) and `evidence/search-ab-context-mech-2026-08-22-baseline-head/`
(baseline copy, saved before worktree removal).

Dynamic (C) cell:

| metric | baseline HEAD | current tree |
| --- | --- | --- |
| model rounds | 57 | 31 |
| tool calls | 81 | 45 |
| failed tool outputs | 12 | 5 |
| provider input tokens | 518,711 | 250,970 |
| wall ms | 362,765 | 228,168 |
| GC forgotten items | 76 | 51 |
| explicit search calls / empty | 8 / 3 | 0 / 0 |

Append (A) cell, same order: rounds 36 → 21, tool calls 50 → 19.

Attribution caveat, recorded deliberately: the A cell is untouched by
the catalog-search change yet swung by a similar fraction
(−42% rounds vs C's −46%), so with n=1 this live pair is **consistent
with no regression plus lower C-cell cost, but does not isolate the
search change**; the fixture's run-to-run variance dominates. The
current-tree run issued zero explicit `context.search` calls (recovery
went through GC reactivation/rereads), so the multi-word recall win
proven in measurements 1–2 had no live pathway in this sample; the
baseline's 8 searches returning 3 empty is consistent with the old
verification-gate weakness. A defensible live attribution needs
`--repeats 3` per cell.

## Cost-reduction fixes landed after the live pair (deterministic, unit-tested)

Event-trace forensics of the dynamic cell (`r1/dynamic/events.jsonl`)
attributed its wasted rounds to three concrete loops:

1. `process.run` spawn failures — 5 consecutive guesses of a
   nonexistent `protocol_tests.exe`/`_v2.exe`. Fix
   (`tool-runtime/src/tools/process.rs`): a spawn `NotFound` now returns
   the typed `path_not_found` failure output with a bounded, sorted cwd
   listing, so one error carries everything needed to correct course.
2. Repeated `capability.manage op=load` (2 of 7 were re-loads; evicted
   tool observations hide the loaded set). Fix
   (`agent-runtime/src/capability/mod.rs`): a load for an already-surfaced
   tool is a cheap no-op (`already_loaded: true`, no reactivation), and
   every load/search response ends with a bounded `session-loaded:` line.
3. Recovery rereads / repeated `fs.read src/protocol.rs` (7 reads): the
   reads follow from scope-close eviction, i.e. context retention policy,
   which is freeze-marked. NOT changed here; reducing it needs an explicit
   policy decision (e.g. lease-on-read via the existing contract
   directives) and touches files owned by the concurrent session.

## Long-flow instrument (`--longflow-run`, efficient by construction)

New development pack `crates/agent-eval/longflow/`
(`late_constraint_long`: 15 turns, early wire constraint, drift turns,
v2 + Heartbeat features, hidden file asserts + reused `wire_v1.py` check)
with runner `agent-eval --longflow-run [id]`. Efficiency comes from
`fixture_driver::compare_mech_live_parallel`: A and C run concurrently in
independent workspaces, so pair wall time approaches a single cell instead
of the sum. Evidence defaults to `crates/agent-eval/evidence/longflow/`.

## Long-flow shakedown (`--longflow-run late_constraint_long`, repeats=1, current tree)

First live run of the concurrent instrument. Pair wall **565 s** ≈
max(dynamic 530 s, append 310 s) — concurrency saved ~275 s versus
sequential. Both cells passed hidden verification (4/4 file asserts +
`wire_v1` command). Dynamic held the mechanism across 15 turns: 127 items
forgotten, 5 recovered via reactivation, first compaction fired
(34.3k → 1.0k tokens), explicit searches fired (8 calls).

| metric | dynamic (C) | append (A) |
| --- | --- | --- |
| rounds / tool calls | 77 / 103 | 48 / 51 |
| failed tool outputs | 14 | 3 |
| provider input tokens | 631,173 | 548,900 |
| active frame tokens | 31,742 | 124,331 |
| wall ms (concurrent pair) | 529,704 | 309,849 |

Two structural findings from event forensics:

1. **Capability churn is idle-cooling, not model blindness.** All 20
   `capability.manage op=load` calls returned "tool loaded" — zero hit
   the new already-loaded no-op — because the unified idle GC cools
   unrooted tools (Loaded → Warm) between rounds of a long flow; inspect
   showed `process.run: warm`. Re-loading is therefore *forced*: the
   ~13 redundant rounds are a ToolLifecycle policy question (anchor
   recently-used tools for N rounds, or widen the production preload),
   not fixable at the dispatcher alone.
2. **Invented-program retries are plan-driven.** The typed
   path_not_found envelope fired correctly every time, with an accurate
   cwd listing (one listing even showed the binary the model kept
   misnaming); the model still cycled spellings (`./`, bare, `.\`,
   `.protocol_tests.exe`) across 8 attempts. Cutting this loop needs
   cross-round repeat escalation in the kernel/runtime, which is outside
   this session's ownership.

Also observed: one `context.manage` round lost to a strict-enum
`invalid_request` (model sent an unknown scope variant) — a candidate
for tolerant parsing or a schema-description tweak. fs.read repeats
reached 21 reads/18 rereads here, reinforcing the read-onlease decision
item below.

## Full-chain waste audit (longflow dynamic cell, quantified)

Cadence: `BeforeModel` maintenance fired x77 (every model round),
AfterModel/UserInput x15 (per turn), AfterTool x11. Average provider
input ~= 8.2k tokens/round, so every saved round is worth ~8k input
tokens directly. Identified waste buckets:

| bucket | evidence | est. rounds | est. input tokens |
| --- | --- | --- | --- |
| capability churn (forced reloads after idle cooling) | 20 loads, all real, zero already-loaded hits | ~13 | ~105k |
| invented-program retry loop | 8x typed path_not_found, 4 spellings incl. one NUL-byte argv | ~7 | ~55k |
| fs.read repeats / recovery rereads | 21 reads, 18 rereads | ~9 | ~75k |
| strict-arg rejections (missing `path` x2, unknown scope variant x1) | serde invalid_request | ~3 | ~25k |

Stability proposals, layered by approval need:

- **L0 (no policy change, dispatcher/contracts only)**: tolerate-and-map
  unknown `ContextScope` variants instead of hard serde rejection;
  enrich `missing field` rejections with the expected arg shape; keep
  argv validation messages naming the offending bytes. Each converts a
  dead-end retry into a same-round self-correction; adds zero tokens.
- **L1-a (ToolLifecycle policy, needs sign-off)**: anchor tools on use
  so idle cooling cannot drop them mid-task (kills the ~13-round churn).
  Cost: kept schemas (~100-165 tokens/tool from surface telemetry)
  persist on later rounds; net still strongly positive vs 8k/round.
- **L1-b (retention policy, freeze-marked, needs explicit approval)**:
  batch closed-scope evictions at turn boundaries (grace within a user
  turn) — targets the 18 rereads; mid-turn frames grow slightly, total
  rounds drop.
- **L1-c (runtime tracking)**: escalate repeated invented-program
  misses after 2 consecutive failures for the same basename.

Validate every item with the paired mech + longflow instruments, n>=2,
comparing rounds/tools/tokens/pass-rate before/after.

## Known boundaries

- Body text is indexed to its first 512 chars; deeper body keywords still
  rely on the residual-scan layer (no regression vs before).
- Engine-only: live-model paired cells (`--compare-live`) are out of scope
  here and refused on a dirty workspace by policy.
- Single synthetic corpus; numbers are diagnostic, not an M15 gate.

## Reproduce

```powershell
cargo test -p context-simple --lib --release search_ab::ab_report -- --nocapture
cargo run -p agent-eval --release -- --retrieval-complex
```
