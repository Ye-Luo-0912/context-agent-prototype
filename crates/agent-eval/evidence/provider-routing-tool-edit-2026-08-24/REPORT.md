# Provider routing Tool Edit diagnostic (2026-08-24)

## Decision

The production file-edit path passed the complete four-cell raw-byte gate with
direct PinAI/Luna. OpenCode `ox-alpha-free` did not provide a stable enough
model stream to serve as acceptance evidence. Use PinAI/Luna for the next
bounded live gates; keep OX as an opportunistic availability smoke until its
upstream `network_error` rate is independently green.

All runs used the same Dynamic engine, production-default tool surface, frozen
Tool Edit V2 fixtures, and a dirty-tree `source_tree_digest`. They are
diagnostics, not formal clean-tree milestone evidence.

| Run | Strict hidden | Tool-edit gate | Rounds | Wall ms | Provider tokens | Complete usage |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| PinAI `gpt-5.6-luna` Responses | 4/4 | 4/4 | 15 | 104,123 | 57,177 | yes |
| Local OpenCode `ox-alpha-free` Auto, initial | 2/4 | 0/4 | 11 | 184,820 | 15,726 | no; 3 cells hit `network_error` |
| Local OpenCode after route circuit | 0/4 | 0/4 | 4 | 76,587 | 0 | no; all 4 cells hit `network_error` |

PinAI completed all edits with zero failed tool outputs. The three ordinary
fixtures used one canonical `edit.patch` on the first valid attempt; the stale
revision fixture followed the accepted proactive reread route. CRLF, mixed
line endings, exact hunk identity, base revisions, two-file atomic coverage,
and hidden commands all passed. There were no shell or whole-file fallbacks and
no post-edit confirmation reads.

The initial OX run still provides useful separation evidence: the two cells
that reached `edit.patch` committed correctly and passed raw-byte hidden
verification, with zero edit failures. One then lost its final model round to
`network_error`; the other made an unnecessary confirmation read and failed
the strict process gate. The remaining two were interrupted before editing.
Thus the editor was not the dominant OX failure.

The relay now preserves terminal network errors, changes the next safe retry's
network path, and rotates only relay-owned upstream sessions. A post-change run
still failed on both direct and system-proxy paths, including after session
rotation. Simple OX text and one-tool probes succeeded between failed gate
runs, pointing to intermittent upstream/model routing rather than a permanent
endpoint, tool-schema, Context, or filesystem incompatibility. More blind OX
retries would inflate rounds and wall time without evidence of higher success.

Evidence bundles:

- `../provider-routing-tool-edit-pinai-luna-2026-08-24/`
- `../provider-routing-tool-edit-ox-2026-08-24/`
- `../provider-routing-tool-edit-ox-circuit-2026-08-24/`
- `../provider-routing-tool-edit-ox-session-2026-08-24/`

