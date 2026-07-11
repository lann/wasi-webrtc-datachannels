#!/usr/bin/env bash
#
# Install the toolchain and dependencies needed to build, run, and test this
# repository. Safe to run repeatedly (idempotent) and shared by local developers, the
# CI workflow (.github/workflows/ci.yml), and the Copilot cloud agent
# (.github/workflows/copilot-setup-steps.yml).
#
# What it installs:
#   - the Rust wasm targets the guest components compile to:
#       * wasm32-unknown-unknown (echo-demo + the manual-signaling test guest)
#       * wasm32-wasip2          (cli-signaling)
#   - wasm-tools, used to wrap guest modules into components and to validate WIT
#   - the Node host's npm dependencies (jco + @roamhq/wrtc)
#
# Prerequisites (not installed here): a Rust toolchain via rustup, and Node 22+
# with npm. CI and copilot-setup-steps provision these before calling this
# script; local developers should install them first.
#
# Environment overrides:
#   WASM_TOOLS_VERSION   version of wasm-tools to `cargo install` (default below)
#   SKIP_NODE=1          skip installing the Node host's npm dependencies

set -euo pipefail

# Resolve the repository root from this script's location so it works from any
# working directory.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

WASM_TOOLS_VERSION="${WASM_TOOLS_VERSION:-1.247.0}"

log() { printf '\n==> %s\n' "$1"; }

log "Adding Rust wasm targets"
rustup target add wasm32-unknown-unknown wasm32-wasip2

log "Ensuring wasm-tools is installed"
if command -v wasm-tools >/dev/null 2>&1; then
  echo "wasm-tools already present: $(wasm-tools --version)"
else
  cargo install --locked wasm-tools --version "${WASM_TOOLS_VERSION}"
fi

if [ "${SKIP_NODE:-0}" = "1" ]; then
  log "Skipping Node host dependencies (SKIP_NODE=1)"
else
  log "Installing Node host dependencies (jco-impl)"
  (cd "${REPO_ROOT}/jco-impl" && npm install)
fi

log "Setup complete"
