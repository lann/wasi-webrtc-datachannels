# wasip3-impl (`wasip3-webrtc-datachannels`)

A **sans-I/O** WebRTC data-channel peer built on `rtc` 0.20 release candidates,
with two interchangeable
drivers: a native UDP reference driver that proves it interoperates with the
repo's `webrtc-rs` stack over a real DTLS + SCTP data channel, and a WASIp3
`wasi:sockets` driver that runs the same core *inside a wasm guest*.

This is the third stack alongside the [`wasmtime-impl`](../wasmtime-impl)
(webrtc-rs) and [`jco-impl`](../jco-impl) (browser) hosts. Instead of the fully
async `webrtc-rs` engine, it drives the *sans-I/O* `rtc` stack, where protocol
logic is separated from I/O. That separation is what lets the same peer run
inside a wasm guest over `wasi:sockets`, realized by the
[`examples/wasip3-cli`](../examples/wasip3-cli) component.

## Layers

- **`SansIoPeer`** (`src/peer.rs`) — the runtime-agnostic core. It wraps an
  `rtc` `RTCPeerConnection` and exposes only signaling primitives, the six
  sans-I/O stepping calls (`poll_transmit` / `handle_input` / `poll_timeout` /
  `handle_timeout` plus drained events), and message sends. It performs **no**
  I/O and awaits nothing.
- **`NativePeer`** (`src/native.rs`, the default `native` feature) — a Tokio
  `UdpSocket` driver that runs the event loop natively. This is the host-side
  driver used to prove the transport against `webrtc-rs`.
- **`GuestPeer`** (`src/guest.rs`, the `guest` feature) — a WASIp3
  `wasi:sockets`/`wasi:clocks` driver that runs the **same** core *inside a wasm
  component*. It exposes a cooperative pump (`flush` / `drain_events` / `wait`)
  rather than a background task, because the component-model async model is
  single-threaded with no cross-thread `spawn`. This is the guest driver
  [`AGENTS.md`](../AGENTS.md) calls the natural next step; the
  [`examples/wasip3-cli`](../examples/wasip3-cli) component drives two of them.

Because `SansIoPeer` is I/O-free, both drivers share it unchanged — the only
difference is where the datagrams and timers come from (Tokio vs. WASIp3).

## Features

- `native` (default) — the Tokio `NativePeer` driver and its `webrtc` interop
  dev-dependency. Off for a wasm guest build, since Tokio does not build for
  `wasm32-wasip2`.
- `guest` — the `GuestPeer` WASIp3 driver (pulls in `wasip3` + `futures`). A
  wasm component enables it with `default-features = false, features =
  ["guest"]`.

## Why the native driver exists alongside the guest one

The native `NativePeer` remains the CI-testable proof of transport interop (a
`webrtc-rs` offerer against the sans-I/O answerer in one process; see the test
below). `GuestPeer` moves the loop into a wasm guest — exercised end-to-end by
`examples/wasip3-cli` under `wasmtime run` — validating that the same core runs
over `wasi:sockets` with no changes.

The sans-I/O model has no OS interface enumeration on wasm, so host candidates
are supplied **explicitly** by the driver via `add_local_host_candidate` rather
than gathered from mDNS.

## Test

```sh
cargo test -p wasip3-webrtc-datachannels --test interop
```

`tests/interop.rs` connects a `webrtc-rs` offerer to this crate's answerer and
round-trips messages in both directions over the data channel. It runs as part
of the workspace `just test`.

## Dependency pin

`rtc` is pinned once, at the workspace level in the root `Cargo.toml`, to
`0.20.0-rc.3`, so native and (future) wasm builds resolve the same source.
