#!/usr/bin/env bash
# Cloud Agent install: bring the default base image up to the toolchains this
# repository pins, then warm the workspace build.
#
# Idempotent by design: it is safe to run repeatedly and against a cached or
# partially prepared VM (e.g. an environment-build snapshot). It installs no
# services and starts no processes.
#
# Pins (kept in lockstep with .github/workflows/ci.yml and global.json):
#   - Rust     1.97.1  (rustfmt + clippy)   -- edition 2024 workspace
#   - .NET SDK 10.0.301                      -- Avalonia desktop + client tests
#   - Python   3.12 (from the base image)   -- docs gate + agent-eval probes
set -euo pipefail

RUST_VERSION="1.97.1"
DOTNET_VERSION="10.0.301"

echo "==> Rust ${RUST_VERSION} (rustfmt, clippy)"
rustup toolchain install "${RUST_VERSION}" \
  --profile minimal --component rustfmt --component clippy --no-self-update
rustup default "${RUST_VERSION}"

echo "==> .NET SDK ${DOTNET_VERSION}"
DOTNET_INSTALL_DIR="${DOTNET_ROOT:-$HOME/.dotnet}"
if [ "$(dotnet --version 2>/dev/null || true)" != "${DOTNET_VERSION}" ]; then
  curl -fsSL https://dot.net/v1/dotnet-install.sh -o /tmp/dotnet-install.sh
  bash /tmp/dotnet-install.sh --version "${DOTNET_VERSION}" --install-dir "${DOTNET_INSTALL_DIR}"
fi
# Expose `dotnet` on PATH for every agent shell without mutating shell profiles:
# CARGO_HOME/bin is already on PATH in the base image, and the .NET host
# resolves its SDK root from the resolved executable location, so a symlink is
# enough (no DOTNET_ROOT export required).
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"
mkdir -p "${CARGO_BIN}"
ln -sf "${DOTNET_INSTALL_DIR}/dotnet" "${CARGO_BIN}/dotnet"

echo "==> Toolchain versions"
rustc --version
cargo --version
dotnet --version
python3 --version

echo "==> Warm the Rust workspace build (also builds agent-host for the .NET chain test)"
cargo build --workspace

echo "==> Install complete"
