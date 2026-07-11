# lann:webrtc-datachannels

A WIT interface and two host implementations showing that high-performance
WebRTC data-channel communication can be expressed with the **WebAssembly
Component Model's async features** (`stream`, `future`, async imports/exports),
with a *single* guest component binary running unchanged against two very
different host stacks:

- a **browser-first** host (Node.js + [`jco`] + [`@roamhq/wrtc`]), and
- a **native Rust** host ([Wasmtime] + [`webrtc-rs`]).

Both hosts run the identical component and round-trip every message through a
genuine WebRTC/SCTP data channel.

[`jco`]: https://github.com/bytecodealliance/jco
[`@roamhq/wrtc`]: https://github.com/WonderInventions/node-webrtc
[Wasmtime]: https://github.com/bytecodealliance/wasmtime
[`webrtc-rs`]: https://github.com/webrtc-rs/webrtc

## What's here

| Path | Deliverable |
| --- | --- |
| [`wit/`](wit) | The streaming **WIT interface**, the `lann:webrtc-datachannels@0.1.0` package. Each demo component keeps its own demo-only WIT and symlinks this package in as a dependency. |
| [`examples/echo-demo`](examples/echo-demo) | A **Rust example component** exercising a data channel entirely through streams. |
| [`wasmtime-impl`](wasmtime-impl) | The **Wasmtime host crate** (webrtc-rs), modeled after `wasmtime_wasi_http::p3`. Provides `add_to_linker` + `WasiWebrtcView` for the `types` + `data-channels` interfaces. Crate name: `wasmtime-webrtc-datachannels`. |
| [`jco-impl`](jco-impl) | The **browser-first host** (Node stand-in for the browser, jco + @roamhq/wrtc). |
| [`examples/wasmtime-demo`](examples/wasmtime-demo) | The **native Rust host** (Wasmtime + webrtc-rs): demo binaries built on `wasmtime-impl`. |
| [`examples/cli-signaling`](examples/cli-signaling) | The **manual-signaling CLI guest component** (Rust). |
| [`AGENTS.md`](AGENTS.md) | Orientation for agents/contributors, linking the `lann/wasm-component-starter` knowledge base. |

## The interface

The interface lives at the root [`wit/`](wit) as the
`lann:webrtc-datachannels` package. Each demo component keeps its own demo-only
WIT alongside it and pulls the package in as a `deps` symlink, so there is still
a single copy of the shared surface to edit:

**`lann:webrtc-datachannels`** — the shared interfaces:

- **`types`** — shared `error` variant and `data-channel-options`.
- **`data-channels`** — the high-throughput surface. A `data-channel` resource
  carries outbound messages as a component-model **stream**, and its inbound
  **stream** is handed back alongside the channel by whatever constructs it
  (for example `connect.open-echo`), so there is no callable-once receive
  method:
  - `send: async func(messages: stream<list<u8>>) -> result<_, error>`
  - the constructor returns `tuple<data-channel, stream<list<u8>>>`

  Each `list<u8>` element is exactly **one** data-channel message, so WebRTC
  message boundaries are preserved end to end. Streaming (rather than one host
  call per message) lets the async ABI pipeline messages and apply
  backpressure.
- **`signaling`** — a fuller `RTCPeerConnection`-style surface (SDP offer/answer
  + trickle ICE) that documents where a *guest-driven* connection API is
  headed. It is the design target and is **not** required by the runnable demo.

**`demo:webrtc-echo`** — the demo-only interfaces, which live with the demo
components that use them ([`examples/echo-demo/wit`](examples/echo-demo/wit)
for the echo demo, [`examples/cli-signaling/wit`](examples/cli-signaling/wit)
for the manual-signaling demos):

- **`connect`** — a convenience used by the demo: `open-echo` returns a channel
  wired to a host-provided echo endpoint, so the example can focus on the
  streaming hot path while still exercising a real WebRTC stack in the host.
- **`rendezvous`** — a proposed, deliberately *unstandardized* HTTP signaling
  mailbox for carrying SDP/ICE between two *separate* peers via an existing
  server over `wasi:http@0.3`, so remote connections can be developed locally.
  Like `signaling`, it is designed but not yet wired into the runnable demo (see
  [`AGENTS.md`](AGENTS.md#real-signaling-rendezvous--wasihttp03--direction)).
- **`demo`** — the exported entry point (`run`) the hosts call.

The demo world is intentionally tiny:

```wit
world webrtc-echo-demo {
    import connect;
    export demo;
}
```

## The example component

[`examples/echo-demo`](examples/echo-demo/src/lib.rs) is host-agnostic Rust
(`wit-bindgen`, `wasm32-unknown-unknown` + `wasm-tools component new`). Its
`run`:

1. calls `connect::open-echo` to get a channel and its inbound stream,
2. spawns a producer that writes `message-count` messages into an outbound
   `stream<list<u8>>`,
3. hands that stream to `data-channel.send`, and **concurrently** reads the
   inbound stream returned by `open-echo` (both under `futures::join!`),
4. returns counts so the host can assert a complete round trip.

## Running it

Prerequisites: Rust (with the `wasm32-unknown-unknown` target),
[`wasm-tools`], Node 22+ (for the Node host). The Node host needs a Node build
with JSPI (`--experimental-wasm-jspi`).

[`wasm-tools`]: https://github.com/bytecodealliance/wasm-tools

### Node (browser-first) host

```sh
cd jco-impl
npm install
npm run build:component   # build the guest .wasm component
npm run transpile         # jco transpile with JSPI async + host maps
node --experimental-wasm-jspi src/run.mjs
```

The host builds two `RTCPeerConnection`s with `@roamhq/wrtc`, performs a real
SDP/ICE handshake, and echoes on the far side.

The same `webrtc.js` host module runs unchanged in a real browser: it resolves
`RTCPeerConnection` from the browser global when present and falls back to
`@roamhq/wrtc` under Node. A headless-Chrome test drives the *identical*
transpiled component through that browser path and is what runs in CI:

```sh
cd jco-impl
npm run build:component && npm run transpile
npm run test:browser     # headless Chrome (137+); set CHROME_PATH to override
```

### Wasmtime (native Rust) host

```sh
cd examples/wasmtime-demo
cargo run --release --bin wasmtime-webrtc-host -- ../echo-demo/build/echo-demo.component.wasm 1000 4096
#                                                                                       ^msg count ^msg size
```

(Run the Node `build:component` step once first, or build the component
manually, to produce the `.wasm`.)
