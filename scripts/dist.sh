#!/usr/bin/env bash
# Build release binaries for the local agent and write SHA-256 checksums.
# Usage: scripts/dist.sh [target-dir]
# Produces dist/<version>/ with agent-tui, agent-host (+ the context
# service binary when present), the published desktop app when the .NET SDK
# is available, SOURCE.txt (source identity) and SHA256SUMS. The Windows
# counterpart is dist.ps1; both must stay behaviorally identical.
set -euo pipefail

target_dir="${1:-target}"
version="$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')"
out="dist/${version}"

# PACKAGE-01: build INTO the same directory the artifacts are copied from
# (--target-dir), so a custom target can never mix stale artifacts from an
# older default-target build into this package.
cargo build --release --bin agent-tui --target-dir "${target_dir}"
cargo build --release --bin agent-host --target-dir "${target_dir}"

# PACKAGE-01: clean staging — a stale file from a previous run (an old
# context-service binary, an old desktop publish) must never ride along.
rm -rf "${out}"
mkdir -p "${out}"
cp "${target_dir}/release/agent-tui" "${out}/agent-tui" 2>/dev/null ||
    cp "${target_dir}/release/agent-tui.exe" "${out}/agent-tui.exe"
cp "${target_dir}/release/agent-host" "${out}/agent-host" 2>/dev/null ||
    cp "${target_dir}/release/agent-host.exe" "${out}/agent-host.exe"
if [ -f "${target_dir}/release/agent-context-service" ] ||
    [ -f "${target_dir}/release/agent-context-service.exe" ]; then
    cargo build --release --bin agent-context-service --target-dir "${target_dir}"
    cp "${target_dir}/release/agent-context-service"* "${out}/" 2>/dev/null || true
fi

# Source identity (R1): what this package is built from, when, and by which
# toolchain. Checksums tie each artifact to this exact build.
source_sha="$(git rev-parse HEAD 2>/dev/null || echo "unknown (no git checkout)")"
cat > "${out}/SOURCE.txt" <<EOF
repository: context-agent-prototype
rust_source_sha: ${source_sha}
cargo_version: ${version}
built_at: $(date -u +%Y-%m-%dT%H:%M:%SZ)
EOF

# The desktop app when the .NET SDK is present; otherwise the package says
# so and the native binaries above remain the deliverable.
if command -v dotnet >/dev/null 2>&1; then
    dotnet publish apps/Agent.Desktop/Agent.Desktop.csproj -c Release -o "${out}/desktop"
    echo "dotnet_sdk: $(dotnet --version)" >> "${out}/SOURCE.txt"
else
    echo "desktop: NOT_RUN (no .NET SDK available)" >> "${out}/SOURCE.txt"
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
