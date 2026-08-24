# Provider routing smoke (2026-08-24)

## Decision

The provider routes are operational and isolated enough for further bounded
testing. This is a short tool-loop smoke, not paired long-flow, Context, or
convergence evidence.

| Route | Model / protocol | Hidden fixture | Wall ms | Model rounds | Tool calls | Failed tool outputs | Edit |
| --- | --- | --- | ---: | ---: | ---: | ---: | --- |
| Direct PinAI `https://api.pinaic.com/v1` | `gpt-5.6-luna` / Responses | `add_test` passed | 35,339 | 7 | 6 | 1 (`process_exit`) | first attempt committed |
| Local OpenCode `http://127.0.0.1:8787/v1` | `ox-alpha-free` / Auto | `add_test` passed | 71,256 | 8 | 7 | 0 | first attempt committed |

The PinAI run used its external Responses base directly. It did not call the
localhost relay. The OpenCode run first received the relay's explicit HTTP 501
capability signal for the upstream's known opaque Responses 500, then cached a
Chat fallback at the same localhost base. The relay health response reported
`responses=true`, `proxy_fallback=true`, and `proxy=system`; its proxy is a
bounded OpenCode upstream retry and is not a provider fallback.

An OpenCode `muse-spark-1.2-contributor` preflight returned HTTP 403 requiring
explicit workspace data-contribution opt-in. The relay correctly did not proxy
retry this account decision. Muse remains unavailable until that opt-in is
made; this is not a link or country-routing failure.

