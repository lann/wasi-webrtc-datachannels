# examples

Guest components (`echo-demo`, `cli-signaling`, `wasip3-cli`) and the native
demo/manual-signaling driver (`wasmtime-demo`) that exercise the
`lann:webrtc-datachannels` interfaces.

- **`echo-demo`** / **`cli-signaling`** — guest components whose WebRTC work is
  performed by a **host** (`wasmtime-impl` or `jco-impl`).
- **`wasip3-cli`** — a self-contained WASIp3 CLI component that runs the whole
  sans-I/O WebRTC stack **in-guest** via `wasip3-impl`'s `GuestPeer`, over
  `wasi:sockets` UDP and `wasi:clocks` timers. It connects an offerer and an
  answerer over loopback and exchanges a message each way. Build and run it with
  `just demo-wasip3-cli` (needs `wasmtime` v46+ on `PATH`), or directly:

  ```sh
  cargo build --release -p wasip3-cli --target wasm32-wasip2
  wasmtime run -W component-model-async=y -S cli -S p3 -S inherit-network \
      target/wasm32-wasip2/release/wasip3_cli.wasm
  ```
- **`wasmtime-demo`** — the native Rust host binaries built on `wasmtime-impl`.

