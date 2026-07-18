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
    cargo clippy --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer -- -D warnings
    cargo clippy -p echo-demo --target wasm32-unknown-unknown -- -D warnings
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

# Run the Rust / Wasmtime tests (includes the manual-signaling integration test).
# nextest runs faster but does not execute doctests, so run those separately.
test:
    cargo nextest run --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer
    cargo test --doc --workspace --exclude echo-demo --exclude cli-signaling --exclude wasip3-webrtc-datachannels --exclude webrtc-consumer

# Run the conformance suite runner over the currently enabled targets. In
# Phase 0 no targets are enabled, so this passes over an empty set; it reads
# conformance/tests.toml + conformance/manifests/ and writes the matrix to
# conformance/matrix.md, exiting nonzero on any fail or unexpected-pass.
conformance:
    cargo run -p conformance-runner -- \
        --tests conformance/tests.toml \
        --manifests conformance/manifests \
        --matrix-out conformance/matrix.md

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
