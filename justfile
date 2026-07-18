# List the available recipes.
default:
    @just --list

# Run every CI check locally, in the same order as .github/workflows/ci.yml.
ci: fmt-check clippy validate-wit build-component transpile test-browser test

# Run the fast pre-commit checks (fmt, clippy, WIT, Rust tests); see AGENTS.md.
check: fmt-check clippy validate-wit test

# Check formatting across all crates.
fmt-check:
    cargo fmt --all -- --check
    cargo fmt --manifest-path wasmtime-impl/tests/manual-signaling-guest/Cargo.toml -- --check

# Run clippy across all crates.
clippy:
    cargo clippy --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer --exclude conformance-guest -- -D warnings
    cargo clippy -p echo-demo --target wasm32-unknown-unknown -- -D warnings
    cargo clippy -p conformance-guest --target wasm32-unknown-unknown -- -D warnings
    cargo clippy -p cli-signaling --target wasm32-wasip2 -- -D warnings
    cargo clippy -p wasip3-webrtc-datachannels --target wasm32-wasip2 -- -D warnings
    cargo clippy -p webrtc-consumer --target wasm32-wasip2 -- -D warnings
    cargo clippy --manifest-path wasmtime-impl/tests/manual-signaling-guest/Cargo.toml --target wasm32-unknown-unknown -- -D warnings

# Validate WIT packages.
validate-wit:
    wasm-tools component wit wit
    wasm-tools component wit examples/echo-demo/wit
    wasm-tools component wit examples/cli-signaling/wit
    wasm-tools component wit wasip3-impl/wit
    wasm-tools component wit examples/webrtc-consumer/wit
    wasm-tools component wit conformance/wit

# Run the Rust / Wasmtime tests (includes the manual-signaling integration test).
# nextest runs faster but does not execute doctests, so run those separately.
test:
    cargo nextest run --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer --exclude conformance-guest
    cargo test --doc --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer --exclude conformance-guest

# Run the conformance suite over the currently enabled targets (see
# conformance/PLAN.md). Builds the shared conformance guest component, runs each
# adapter to produce conformance/results/<target>.json, then runs the runner
# over conformance/tests.toml + conformance/manifests/ + those results — also
# spawning the standalone conformance-signalingd binary (ephemeral localhost
# port, gated on /healthz) to exercise its lifecycle — and writes the matrix to
# conformance/matrix.md, exiting nonzero on any fail or unexpected-pass.
#
# Enabled targets: wasmtime (native webrtc-rs), jco-node and jco-browser (the
# browser-first host transpiled by jco, run under Node and headless Chromium),
# plus the wasmtime<->jco-node interop pair. The jco targets need a JSPI-capable
# Node (24+; see conformance-jco-node) and, for the browser target, a Chrome
# 137+ binary (auto-detected, or set CHROME_PATH).
conformance: build-conformance-guest transpile-conformance-guest
    cargo build -p conformance-signalingd
    cargo run --release -p conformance-adapter-wasmtime --bin conformance-adapter-wasmtime -- \
        --guest conformance/guest/build/conformance-guest.component.wasm \
        --out conformance/results
    just conformance-jco-node
    just conformance-jco-browser
    just conformance-interop
    cargo run -p conformance-runner -- \
        --tests conformance/tests.toml \
        --manifests conformance/manifests \
        --results conformance/results \
        --signaling-bin target/debug/conformance-signalingd \
        --matrix-out conformance/matrix.md

# Build the shared conformance guest component into conformance/guest/build/.
build-conformance-guest:
    cargo build --release -p conformance-guest --target wasm32-unknown-unknown
    mkdir -p conformance/guest/build
    wasm-tools component new \
        target/wasm32-unknown-unknown/release/conformance_guest.wasm \
        -o conformance/guest/build/conformance-guest.component.wasm

# Transpile the conformance guest for the jco adapters (Node + browser) into
# conformance/adapters/jco/generated (jco `--instantiation` mode so one process
# can stand up two guest instances). Rebuilds the guest component first.
transpile-conformance-guest: build-conformance-guest
    cd conformance/adapters/jco && npm run transpile

# Run the jco-node conformance adapter (guest transpiled by jco, run under Node
# with @roamhq/wrtc). jco's async ABI needs JavaScript Promise Integration, so
# this uses `node --experimental-wasm-jspi` and requires Node 24+ (which ships
# WebAssembly.Suspending). Writes conformance/results/jco-node.json.
conformance-jco-node: transpile-conformance-guest
    cargo build -p conformance-signalingd
    cd conformance/adapters/jco && npm run run:node

# Run the jco-browser conformance adapter (the same guest + host modules inside
# headless Chromium; Chrome 137+ ships JSPI). Writes
# conformance/results/jco-browser.json.
conformance-jco-browser: transpile-conformance-guest
    cargo build -p conformance-signalingd
    cd conformance/adapters/jco && npm run run:browser

# Run the wasmtime<->jco-node interop pair (both orders): one peer per runtime
# shares a signaling room and a real WebRTC data channel. Writes
# conformance/results/wasmtime-x-jco-node.json and
# conformance/results/jco-node-x-wasmtime.json.
conformance-interop: transpile-conformance-guest
    cargo build -p conformance-signalingd
    cargo run --release -p conformance-adapter-wasmtime --bin conformance-interop

# Build the echo-demo guest component into examples/echo-demo/build/.
build-component:
    cd jco-impl && npm run build:component

# Transpile the echo-demo component for the Node host (runs build-component).
transpile: build-component
    cd jco-impl && npm run transpile

# Run the browser host test in headless Chrome (set CHROME_PATH to override).
test-browser: transpile
    cd jco-impl && npm run test:browser

# Run the Node (browser-first) host demo.
demo-node: transpile
    cd jco-impl && node --experimental-wasm-jspi src/run.mjs

# Run the Wasmtime (native) host demo.
demo-wasmtime count="1000" size="4096": build-component
    cargo run --release --bin wasmtime-webrtc-host -- \
        examples/echo-demo/build/echo-demo.component.wasm {{count}} {{size}}

# Build the wasip3 provider component (the whole WebRTC stack runs in-guest;
# it exports `lann:webrtc-datachannels/connections`) into
# target/wasm32-wasip2/release/wasip3_webrtc_datachannels.wasm.
build-wasip3-provider:
    cargo build --release -p wasip3-webrtc-datachannels --target wasm32-wasip2

# Build the webrtc-consumer component (imports `connections`) into
# target/wasm32-wasip2/release/webrtc_consumer.wasm.
build-webrtc-consumer:
    cargo build --release -p webrtc-consumer --target wasm32-wasip2

# Compose the consumer with the provider (`wac plug`) into
# target/webrtc-composed.wasm. The consumer's `connections` import is satisfied
# by the provider's export, yielding one self-contained component.
compose-webrtc: build-wasip3-provider build-webrtc-consumer
    wac plug target/wasm32-wasip2/release/webrtc_consumer.wasm \
        --plug target/wasm32-wasip2/release/wasip3_webrtc_datachannels.wasm \
        -o target/webrtc-composed.wasm

# Basic in-guest integration test: run the composed consumer+provider under
# `wasmtime`, standing up two peers that connect over loopback entirely inside
# wasm and exchange a message each way. Requires `wasmtime` (v46+) and `wac`.
# `timeout` bounds the whole run so a stuck handshake fails the recipe instead
# of hanging CI; the guest itself also bounds `wait-connected` internally.
test-webrtc-composed: compose-webrtc
    timeout 120 wasmtime run -W component-model-async=y -S cli -S p3 -S inherit-network \
        target/webrtc-composed.wasm
