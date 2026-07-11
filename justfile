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
    cargo clippy --workspace --exclude echo-demo --exclude cli-signaling -- -D warnings
    cargo clippy -p echo-demo --target wasm32-unknown-unknown -- -D warnings
    cargo clippy -p cli-signaling --target wasm32-wasip2 -- -D warnings
    cargo clippy --manifest-path wasmtime-impl/tests/manual-signaling-guest/Cargo.toml --target wasm32-unknown-unknown -- -D warnings

# Validate WIT packages.
validate-wit:
    wasm-tools component wit wit
    wasm-tools component wit examples/echo-demo/wit
    wasm-tools component wit examples/cli-signaling/wit

# Run the Rust / Wasmtime tests (includes the manual-signaling integration test).
# nextest runs faster but does not execute doctests, so run those separately.
test:
    cargo nextest run --workspace --exclude echo-demo --exclude cli-signaling
    cargo test --doc --workspace --exclude echo-demo --exclude cli-signaling

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
