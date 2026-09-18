# Failed-turn directive durability repair

## Proven cause

The DurableQueue run accepted correction input `beaf04ae-157d-43d2-bfaf-9be56c6e782f`
in run `0a473aca-04fd-450b-a71c-2e5bf839b4a7`. It appeared in the actual model input,
and its input lifecycle was recorded, but that run produced zero durable
checkpoints before a malformed model response ended it. The next process restored
run `ad1cb297-e24d-43fc-9b6a-3aca53f56bc7` and lost that correction.

The missing boundary was between input/event persistence and resumable task state:
`settle_failed_turn` dropped the ActiveTurn without installing/capturing the newest
task directive and execution facts. Unlike the deliberate round-budget exit, a
provider failure did not owe a resumable snapshot. `Consumed` is input lifecycle
accounting, not proof that restore=latest contains the directive. No parser of
model text, cache hint, or free-text plan can repair that durability gap.

Read scope: the affected failure lifecycle, model/tool completion callers,
safe-point writer/barriers, parked maintenance continuations, directive admission
and resolution, and their tests. This was not a new full-repository audit.

## Implementation

- Add typed `CheckpointDebtReason::FailedTurnYield`.
- At the shared failed-turn exit, admit the checkpoint path only for an active
  task with a settled batch, no live operation/pending tools/cleanup, healthy
  Runtime/Core state and a configured checkpoint store. The round-budget path
  retains its existing explicit durable-stop barrier.
- Drain an older checkpoint first, install the bounded execution state through
  the existing safe-point method, capture all planes using the existing authority
  marker and validation, then require the durability watermark/debt gate before
  publishing `TurnFailed` and discarding the turn.
- Reuse the existing checkpoint relay and `GcContinuation` lane for a failure
  tail. Slow maintenance therefore leaves commands responsive. A cancelled tail
  cannot resurrect through a late relay completion.
- The relay refuses to overwrite an occupied boundary lane; that rare collision
  falls back to the existing checkpoint barrier. Failed maintenance is rejected
  by the shared writer rather than being mistaken for an absent optional report.
  The gated responsiveness test covers the asynchronous free-lane path, not an
  arbitrary blocked engine in the occupied-lane fallback.
- On checkpoint maintenance/capture/write failure, retain debt and expose a
  recovery fence. Do not publish a false `CheckpointDurable`, close the task,
  re-admit the directive, or automatically replay an uncertain action.

The storage-free configuration keeps its existing ephemeral behavior. The change
does not promise survival of an arbitrary kill before a durable barrier, nor does
it repair malformed JSON from a supplier. Text already published remains a retry
barrier; the repaired failure path makes subsequent explicit continuation coherent.

## Executed regression evidence

`tests/turn/failure_resume.rs` exercises the real Runtime, SimpleContextEngine,
file event/authority journals, checkpoint store and a fresh Runtime instance.
The initial malformed-output test failed before the production change because no
new durable checkpoint preceded `TurnFailed` (`target/failure-resume-20260918/red.log`).

Eight final focused tests pass:

1. malformed model arguments preserve a new correction across cold restore;
2. provider transport failure does the same without automatic call replay;
3. output-budget failure does the same;
4. input-budget refusal preserves the correction before any provider call;
5. a blocked checkpoint store fences instead of claiming durability;
6. failed checkpoint maintenance produces no resumable snapshot;
7. a gated maintenance pass keeps status/cancellation responsive and its stale
   completion cannot publish a second terminal;
8. an older in-flight prepare drains before the failed-turn snapshot, with two
   strictly increasing durable sequence acknowledgements.

The cold-restore tests also assert unchanged TaskId/directive epoch, no duplicate
Dialogue admission, no TaskCompleted, and preservation of reported failure usage.
The correction exceeds the input-preview bound; its full digest and the actual
restored model request, including the beyond-preview suffix, are checked.

Full `cargo test -p agent-runtime` passed on the production patch with the first
seven new tests (760 tests across targets). The additional older-prepare regression
then passed with the eight-test focused target; it changed test code only.
Workspace all-target check and Runtime all-target Clippy `-D warnings` passed.
Compose/TUI consumers: 178 passed, 7 existing ignored tests, zero failures. The
single-slot/writer guard follow-up is checked again on the affected Runtime lib
and turn targets (436 + 154 passed) plus the final real-binary replay. No remote CI
or Unix result is claimed.

## Real-binary process boundary

`target/failure-resume-20260918/repro_headless.py` launches three independent
agent-tui processes against a local SSE fixture, with the real Dynamic composition
and persistent authority journal. It replays the previously captured malformed
Responses body; usage is replaced with local synthetic counters, not billed usage.

- Process 1 creates the prior durable task (sequence 1).
- Process 2 accepts `NEW-CORRECTION-917`, receives malformed arguments after text
  deltas, dispatches no malformed tool, writes sequence 2, then exits with the
  original model-failure class.
- Process 3 restores sequence 2; its first actual request contains the complete
  correction, and it finishes without new user Dialogue admission or task acceptance.

`headless-result.json` records PASS, binary SHA-256, checkpoint identities and
restore source runs. This is a deterministic local real-binary replay, not a new
paid DeepSeek run or a general model-quality benchmark.

## DurableQueue artifact repairs

Separately, the ignored evaluation workspace now rejects unstripped identifiers,
tuple payloads and duplicate submitted ids within a batch. Cross-batch idempotence
is retained. The generated whitespace self-test had asserted acceptance contrary
to TASK.md; its assertion was corrected and three reviewer regressions added.
DESIGN.md was supplied and README records the provenance and actual validation.

Post-repair checks: public 25/25, application regressions 34/34, independent reviewer
18/18, supplementary concurrency/atomicity probes 3/3: 80 passing tests. Original
failed-run artifacts, autonomous source snapshot and fixed inputs remain intact.
This is an outer reviewer repair, not retroactive success of the autonomous run.

No commit/push/release was made. The previous uncommitted work was preserved.
