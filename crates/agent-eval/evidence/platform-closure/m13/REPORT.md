# Closure evidence — structured attestation and fail-closed activation

Schema `platform-closure.m13.v1`. Generated mechanically by `agent-eval --platform-closure-m13`; activation/refusal rows executed real children inside this run.

| metric | value |
| --- | --- |
| rows | 8 |
| activated | 2 |
| refused | 4 |
| explicit not_run | 2 |
| unresolved | 0 |

## Coverage

| row | platform/backend | profile | required ⊆ actual | validate | result | reason |
| --- | --- | --- | --- | --- | --- | --- |
| activation/trusted/minimal-floor | windows/integrity+jobobject v1 | Trusted | true | Ok | activated | - |
| activation/restricted/full-floor | windows/integrity+jobobject v1 | Restricted | true | Ok | activated | - |
| activation/restricted/missing-write-confinement | windows/integrity+jobobject v1 | Restricted | false | Ok | refused | context error: capability 'process-demo' sandbox profile Restricted is not covered by enforced SandboxCapabilities { fs_read_confined: false, fs_write_confined: false, tcp_connect_denied: false, udp_denied: false, unix_socket_denied: false, process_count_quota: true, signal_scoped: false, cpu_quota: false, memory_quota: true, fd_quota: false } (backend integrity+jobobject 1) |
| activation/untrusted-generated/native-refusal | windows/integrity+jobobject v1 | UntrustedGenerated | false | Ok | refused | context error: capability 'process-demo' sandbox profile UntrustedGenerated is not covered by enforced SandboxCapabilities { fs_read_confined: false, fs_write_confined: true, tcp_connect_denied: false, udp_denied: false, unix_socket_denied: false, process_count_quota: true, signal_scoped: false, cpu_quota: false, memory_quota: true, fd_quota: false } (backend integrity+jobobject 1) |
| contract/attestation/true-flag-requires-proof | contract | Restricted |  | Err as required (Err("sandbox evidence disagrees with the enforced capability flags")) | refused | - |
| contract/attestation/backend-label-required | contract | Restricted |  | Err(empty backend label) | refused | - |
| other-platform/linux/Trusted/observation | linux/landlock+rlimits (windows/integrity+jobobject v1 runner) | Trusted | not_run(platform) | not_run(platform) | not_run(platform) | landlock-backed attestation executes only where the kernel plane runs; covered by the referenced platform-gated deterministic suite |
| other-platform/linux/Restricted/observation | linux/landlock+rlimits (windows/integrity+jobobject v1 runner) | Restricted | not_run(platform) | not_run(platform) | not_run(platform) | landlock-backed attestation executes only where the kernel plane runs; covered by the referenced platform-gated deterministic suite |

## Gates

- zero unexplained activation/refusal rows and zero unresolved rows: true
- every activated case validated its attestation and carried non-empty mechanism proofs: true
- native untrusted floor refuses when its complete floor cannot be attested: true

**Verdict: PASS**
