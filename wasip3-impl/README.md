# wasip3-impl (`wasip3-webrtc-datachannels`)

A **sans-I/O** WebRTC data-channel peer built on the wasm-capable
[`lann/rtc`](https://github.com/lann/rtc/tree/wasi) fork, plus a native UDP
reference driver that proves it interoperates with the repo's `webrtc-rs`
stack over a real DTLS + SCTP data channel.

This is the third stack alongside the [`wasmtime-impl`](../wasmtime-impl)
(webrtc-rs) and [`jco-impl`](../jco-impl) (browser) hosts. Instead of the fully
async `webrtc-rs` engine, it drives the *sans-I/O* `rtc` stack, where protocol
logic is separated from I/O. That separation is what will let the same peer run
inside a wasm guest over `wasi:sockets` — the direction the `rtc` `wasi` fork
unblocks (see [`AGENTS.md`](../AGENTS.md)).

## Layers

- **`SansIoPeer`** (`src/peer.rs`) — the runtime-agnostic core. It wraps an
  `rtc` `RTCPeerConnection` and exposes only signaling primitives, the six
  sans-I/O stepping calls (`poll_transmit` / `handle_input` / `poll_timeout` /
  `handle_timeout` plus drained events), and message sends. It performs **no**
  I/O and awaits nothing.
- **`NativePeer`** (`src/native.rs`) — a Tokio `UdpSocket` driver that runs the
  event loop. This is the host-side driver used to prove the transport; a future
  guest driver would feed `SansIoPeer` from `wasi:sockets` instead.

## Why the driver is host-side (for now)

The sans-I/O loop lives host-side because that is what is CI-testable today: the
round-trip test stands up a `webrtc-rs` offerer and this crate's answerer in one
process. Because `SansIoPeer` is I/O-free, moving the loop into a wasm guest
later is a matter of writing a `wasi:sockets`/timer driver against the same core.

The sans-I/O model has no OS interface enumeration (the fork stubs `ifaces()` to
return `Unsupported` on wasm), so host candidates are supplied **explicitly** by
the driver via `add_local_host_candidate` rather than gathered from mDNS.

## Test

```sh
cargo test -p wasip3-webrtc-datachannels --test interop
```

`tests/interop.rs` connects a `webrtc-rs` offerer to this crate's answerer and
round-trips messages in both directions over the data channel. It runs as part
of the workspace `just test`.

## Dependency pin

`rtc` is pinned once, at the workspace level in the root `Cargo.toml`, to the
`lann/rtc` `wasi` fork by commit, so native and (future) wasm builds resolve the
same source.
