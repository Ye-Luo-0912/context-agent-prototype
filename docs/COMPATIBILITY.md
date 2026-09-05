# Compatibility statement

What a consumer of this repository's artifacts may rely on across
versions, and what is explicitly allowed to change. This is the V1
compatibility declaration; each section names the owning artifact.

## Runtime checkpoints

- Artifact: `.focus-agent/checkpoints/checkpoint-<ms>-<runid>.json`,
  written only by `CheckpointStore` as a
  `runtime-checkpoint-envelope-v1` envelope (header line: format,
  SHA-256, payload length; then the payload).
- The payload carries `version: RUNTIME_CHECKPOINT_VERSION` (currently
  **4**). A different version is rejected with an explicit error;
  automatic migration does not exist and is not planned for V1.
- Non-breaking today: new optional payload fields are serde-defaulted
  (ack debts, event cursor, snapshot sequence, provider profile digest).
- Legacy raw-JSON checkpoints (pre-envelope manual exports) remain
  readable through the legacy decoder, but are never written again.
- Retention covers only real store artifacts; foreign files in the
  directory are never counted or deleted.

## Runtime events

- Artifact: `.focus-agent/traces/*.jsonl` — one
  `RuntimeEventEnvelope` per line, internally serde-tagged by `type`.
- Append-only by construction: new variants may appear; existing
  variants do not change meaning. Consumers (TUI projection, replay,
  eval metrics) must treat unknown variants as ignorable — this is the
  contract that keeps old traces replayable by newer binaries and vice
  versa.
- Deleting a variant or reusing a name with different fields is a
  breaking change and requires a compatibility note here plus a version
  bump of the producing binary.

## Provider wire profile

- The key-free serving identity is the `provider_profile_digest`
  (SHA-256 over schema `provider-profile.v1`): base URL, model,
  protocol, context window, max output tokens, and the sampling policy
  (declared provider-default or a pinned temperature). It is printed at
  startup and persisted into every checkpoint's `RunMetadata`.
- Two runs compare operating points by comparing digests; a digest
  change means the measured object changed.
- The digest intentionally excludes the API key and retry/timeout knobs.

## Shadow Context Frame

- Artifact: `RuntimeEvent::ContextFrameShadow` carrying a bounded JSON
  manifest of schema `context-frame-shadow/v1`.
- Measurement-only: enabling `shadow_context_frame` never changes the
  model input. The manifest schema may grow (new optional fields);
  existing field meanings are frozen.
- `agent-replay --frame-gate` enforces the invariant subset over
  recorded manifests and may be used as a CI gate.

## Tools and effect authority

- ToolSpec is a model-visible schema, not authority. Host tool policies
  bind builtin arguments to EffectIntents; a plugin cannot self-
  authorize. Policy revisions are stamped on authority leases.
- The change journal (`.focus-agent/changes.jsonl`) records every
  durable workspace mutation with its preparing call — review/revert
  substrate; treat it as append-only.

## Explicitly unstable (may change without notice)

- Anything under `docs/archive/`.
- The TUI transcript rendering (bounded window; the durable transcript
  is the event journal).
- Diagnostic text embedded in warnings and errors (match on typed
  events, never on prose).
- `docs/state.json` field additions (removals/rename are announced).
