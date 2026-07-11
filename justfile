# Run all CI checks locally.
ci: fmt-check clippy validate-wit test

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

# Run tests.
test:
    cargo test --workspace --exclude echo-demo --exclude cli-signaling
