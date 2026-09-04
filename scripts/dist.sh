#!/usr/bin/env bash
# Build release binaries for the local agent and write SHA-256 checksums.
# Usage: scripts/dist.sh [target-dir]
# Produces dist/<version>/ with agent-tui (+ the context service binary
# when present) and SHA256SUMS. The Windows counterpart is dist.ps1; both
# must stay behaviorally identical.
set -euo pipefail

target_dir="${1:-target}"
version="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
out="dist/${version}"

cargo build --release --bin agent-tui
mkdir -p "${out}"
cp "${target_dir}/release/agent-tui" "${out}/agent-tui" 2>/dev/null ||
    cp "${target_dir}/release/agent-tui.exe" "${out}/agent-tui.exe"
if [ -f "${target_dir}/release/agent-context-service" ] ||
    [ -f "${target_dir}/release/agent-context-service.exe" ]; then
    cargo build --release --bin agent-context-service
    cp "${target_dir}/release/agent-context-service"* "${out}/" 2>/dev/null || true
fi

(
    cd "${out}"
    if command -v sha256sum >/dev/null 2>&1; then
        find . -type f ! -name SHA256SUMS -exec sha256sum {} \; > SHA256SUMS
    else
        find . -type f ! -name SHA256SUMS -exec shasum -a 256 {} \; > SHA256SUMS
    fi
)
echo "dist ready: ${out}"
cat "${out}/SHA256SUMS"
