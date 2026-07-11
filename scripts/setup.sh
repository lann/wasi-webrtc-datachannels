#!/usr/bin/env bash
#
# Install the toolchain and dependencies needed to build, run, and test this
# repository. Safe to run repeatedly (idempotent) and shared by local developers, the
# CI workflow (.github/workflows/ci.yml), and the Copilot cloud agent
# (.github/workflows/copilot-setup-steps.yml).
#
# What it installs:
#   - the pinned Rust toolchain and the wasm targets the guest components compile
#     to, as declared in rust-toolchain.toml:
#       * wasm32-unknown-unknown (echo-demo + the manual-signaling test guest)
#       * wasm32-wasip2          (cli-signaling)
#   - wasm-tools, used to wrap guest modules into components and to validate WIT
#   - just, the command runner used for development and CI recipes
#   - cargo-nextest, the faster test runner used by `just test`
#   - the Node host's npm dependencies (jco + @roamhq/wrtc)
#
# wasm-tools, just, and cargo-nextest are installed with cargo-binstall, which
# downloads the pinned prebuilt release binaries when available and automatically
# falls back to `cargo install` (compiling from source) otherwise. cargo-binstall
# itself is bootstrapped from its prebuilt release binary.
#
# Prerequisites (not installed here): a Rust toolchain via rustup, and Node 22+
# with npm. CI and copilot-setup-steps provision these before calling this
# script; local developers should install them first.
#
# Environment overrides:
#   WASM_TOOLS_VERSION   version of wasm-tools to install (default below)
#   JUST_VERSION         version of just to install (default below)
#   NEXTEST_VERSION      version of cargo-nextest to install (default below)
#   SKIP_NODE=1          skip installing the Node host's npm dependencies

set -euo pipefail

# Resolve the repository root from this script's location so it works from any
# working directory.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

WASM_TOOLS_VERSION="${WASM_TOOLS_VERSION:-1.247.0}"
JUST_VERSION="${JUST_VERSION:-1.40.0}"
NEXTEST_VERSION="${NEXTEST_VERSION:-0.9.140}"

log() { printf '\n==> %s\n' "$1"; }

log "Installing pinned Rust toolchain and wasm targets (rust-toolchain.toml)"
# Running rustup in the repo root installs the toolchain, targets, and
# components declared in rust-toolchain.toml.
(cd "${REPO_ROOT}" && rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install)

log "Ensuring cargo-binstall is installed"
if command -v cargo-binstall >/dev/null 2>&1; then
  echo "cargo-binstall already present: $(cargo-binstall -V)"
else
  # Bootstrap cargo-binstall from its prebuilt release binary; this installer
  # drops the binary into the cargo bin dir.
  curl -fsSL --proto '=https' --tlsv1.2 \
    https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
fi

# Install a crate binary with cargo-binstall. It fetches a prebuilt artifact when
# one exists and otherwise falls back to `cargo install` automatically.
binstall() {
  cargo binstall --no-confirm --locked "$1"
}

log "Ensuring wasm-tools ${WASM_TOOLS_VERSION} is installed"
if command -v wasm-tools >/dev/null 2>&1; then
  echo "wasm-tools already present: $(wasm-tools --version)"
else
  binstall "wasm-tools@${WASM_TOOLS_VERSION}"
fi

log "Ensuring just ${JUST_VERSION} is installed"
if command -v just >/dev/null 2>&1; then
  echo "just already present: $(just --version)"
else
  binstall "just@${JUST_VERSION}"
fi

log "Ensuring cargo-nextest ${NEXTEST_VERSION} is installed"
if command -v cargo-nextest >/dev/null 2>&1; then
  echo "cargo-nextest already present: $(cargo-nextest --version)"
else
  binstall "cargo-nextest@${NEXTEST_VERSION}"
fi

if [ "${SKIP_NODE:-0}" = "1" ]; then
  log "Skipping Node host dependencies (SKIP_NODE=1)"
else
  log "Installing Node host dependencies (jco-impl)"
  (cd "${REPO_ROOT}/jco-impl" && npm install)
fi

log "Setup complete"
