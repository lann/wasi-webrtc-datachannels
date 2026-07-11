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
#   - the Node host's npm dependencies (jco + @roamhq/wrtc)
#
# wasm-tools and just are installed from prebuilt release binaries (pinned to the
# versions below) to avoid compiling them from source on cold caches; the
# `cargo install` path is kept as a fallback for platforms without a prebuilt
# artifact.
#
# Prerequisites (not installed here): a Rust toolchain via rustup, and Node 22+
# with npm. CI and copilot-setup-steps provision these before calling this
# script; local developers should install them first.
#
# Environment overrides:
#   WASM_TOOLS_VERSION   version of wasm-tools to install (default below)
#   JUST_VERSION         version of just to install (default below)
#   SKIP_NODE=1          skip installing the Node host's npm dependencies

set -euo pipefail

# Resolve the repository root from this script's location so it works from any
# working directory.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

WASM_TOOLS_VERSION="${WASM_TOOLS_VERSION:-1.247.0}"
JUST_VERSION="${JUST_VERSION:-1.40.0}"

log() { printf '\n==> %s\n' "$1"; }

# Directory prebuilt binaries are installed into: the cargo bin dir, which is on
# PATH and cached in CI.
BIN_DIR="${CARGO_HOME:-${HOME}/.cargo}/bin"
mkdir -p "${BIN_DIR}"

# Detect the OS/arch and map it to the platform strings used by each project's
# release artifacts. Echoes "<wasm_tools_platform> <just_platform>", or returns
# non-zero when the platform is unknown (callers then fall back to
# `cargo install`).
detect_platform() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "${arch}" in
    x86_64 | amd64) arch="x86_64" ;;
    aarch64 | arm64) arch="aarch64" ;;
    *) return 1 ;;
  esac
  case "${os}" in
    # wasm-tools uses "<arch>-linux"; just uses a musl target triple.
    Linux) echo "${arch}-linux ${arch}-unknown-linux-musl" ;;
    Darwin) echo "${arch}-macos ${arch}-apple-darwin" ;;
    *) return 1 ;;
  esac
}

# install_from_tarball <url> <path-within-archive> <dest-binary-name>
# Downloads and extracts a single binary from a .tar.gz release artifact into
# BIN_DIR. Returns non-zero on any failure so callers can fall back.
install_from_tarball() {
  local url="$1" member="$2" name="$3" tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "${tmp}"' RETURN
  if ! curl -fsSL "${url}" -o "${tmp}/archive.tar.gz"; then
    return 1
  fi
  if ! tar -xzf "${tmp}/archive.tar.gz" -C "${tmp}"; then
    return 1
  fi
  if [ ! -f "${tmp}/${member}" ]; then
    return 1
  fi
  install -m 0755 "${tmp}/${member}" "${BIN_DIR}/${name}"
}

log "Installing pinned Rust toolchain and wasm targets (rust-toolchain.toml)"
# Running rustup in the repo root installs the toolchain, targets, and
# components declared in rust-toolchain.toml.
(cd "${REPO_ROOT}" && rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install)

log "Ensuring wasm-tools ${WASM_TOOLS_VERSION} is installed"
if command -v wasm-tools >/dev/null 2>&1; then
  echo "wasm-tools already present: $(wasm-tools --version)"
else
  installed=0
  if platforms="$(detect_platform)"; then
    wt_platform="${platforms%% *}"
    wt_dir="wasm-tools-${WASM_TOOLS_VERSION}-${wt_platform}"
    wt_url="https://github.com/bytecodealliance/wasm-tools/releases/download/v${WASM_TOOLS_VERSION}/${wt_dir}.tar.gz"
    if install_from_tarball "${wt_url}" "${wt_dir}/wasm-tools" wasm-tools; then
      echo "installed wasm-tools from prebuilt binary: $(wasm-tools --version)"
      installed=1
    else
      echo "prebuilt wasm-tools download failed; falling back to cargo install"
    fi
  fi
  if [ "${installed}" -eq 0 ]; then
    cargo install --locked wasm-tools --version "${WASM_TOOLS_VERSION}"
  fi
fi

log "Ensuring just ${JUST_VERSION} is installed"
if command -v just >/dev/null 2>&1; then
  echo "just already present: $(just --version)"
else
  installed=0
  if platforms="$(detect_platform)"; then
    just_platform="${platforms##* }"
    just_url="https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-${just_platform}.tar.gz"
    if install_from_tarball "${just_url}" just just; then
      echo "installed just from prebuilt binary: $(just --version)"
      installed=1
    else
      echo "prebuilt just download failed; falling back to cargo install"
    fi
  fi
  if [ "${installed}" -eq 0 ]; then
    cargo install --locked just --version "${JUST_VERSION}"
  fi
fi

if [ "${SKIP_NODE:-0}" = "1" ]; then
  log "Skipping Node host dependencies (SKIP_NODE=1)"
else
  log "Installing Node host dependencies (jco-impl)"
  (cd "${REPO_ROOT}/jco-impl" && npm install)
fi

log "Setup complete"
