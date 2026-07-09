# wasi:webrtc-data-channels — feasibility spike

An exploratory **spike** validating that high-performance WebRTC data-channel
communication can be expressed with the **WebAssembly Component Model's async
features** (`stream`, `future`, async imports/exports), and that a *single*
guest component binary can run unchanged against two very different host
stacks:

- a **browser-first** host (Node.js + [`jco`] + [`@roamhq/wrtc`]), and
- a **native Rust** host ([Wasmtime] + [`webrtc-rs`]).

This is a spike, not a product: the interface and implementations are meant to
be iterated on. The goal was to answer *"is this technically feasible, and what
does it cost?"* — and the answer is **yes** (see [Findings](#findings)).

[`jco`]: https://github.com/bytecodealliance/jco
[`@roamhq/wrtc`]: https://github.com/WonderInventions/node-webrtc
[Wasmtime]: https://github.com/bytecodealliance/wasmtime
[`webrtc-rs`]: https://github.com/webrtc-rs/webrtc

## What's here

| Path | Deliverable |
| --- | --- |
| [`wit/`](wit) | The streaming **WIT interface**, split into the reusable `wasi:webrtc-data-channels@0.1.0` and demo-only `demo:webrtc-echo@0.1.0` packages. |
| [`components/echo-demo`](components/echo-demo) | A **Rust example component** exercising a data channel entirely through streams. |
| [`hosts/node`](hosts/node) | The **browser-first host** (Node stand-in for the browser). |
| [`hosts/wasmtime`](hosts/wasmtime) | The **native Rust host** (Wasmtime + webrtc-rs). |
| [`AGENTS.md`](AGENTS.md) | Orientation for agents/contributors, linking the `lann/wasm-component-starter` knowledge base. |

The same `echo-demo.component.wasm` produced from `components/echo-demo` is
loaded by **both** hosts. That is the core compatibility result of the spike.

## The interface

The WIT lives in one place, [`wit/`](wit), and is split into two packages that
separate the reusable surface from the demo-only glue:

**`wasi:webrtc-data-channels`** — the reusable interfaces:

- **`types`** — shared `error` variant and `data-channel-options`.
- **`data-channels`** — the high-throughput surface. A `data-channel` resource
  carries both directions as component-model **streams**:
  - `send: async func(messages: stream<list<u8>>) -> result<_, error>`
  - `receive: async func() -> stream<list<u8>>`

  Each `list<u8>` element is exactly **one** data-channel message, so WebRTC
  message boundaries are preserved end to end. Streaming (rather than one host
  call per message) lets the async ABI pipeline messages and apply
  backpressure.
- **`signaling`** — a fuller `RTCPeerConnection`-style surface (SDP offer/answer
  + trickle ICE) that documents where a *guest-driven* connection API is
  headed. It is the design target and is **not** required by the runnable demo.

**`demo:webrtc-echo`** — the demo-only interfaces:

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

[`components/echo-demo`](components/echo-demo/src/lib.rs) is host-agnostic Rust
(`wit-bindgen`, `wasm32-unknown-unknown` + `wasm-tools component new`). Its
`run`:

1. calls `connect::open-echo` to get a channel,
2. spawns a producer that writes `message-count` messages into an outbound
   `stream<list<u8>>`,
3. hands that stream to `data-channel.send`, and **concurrently** reads the
   inbound stream from `data-channel.receive` (both under `futures::join!`),
4. returns counts so the host can assert a complete round trip.

## Running it

Prerequisites: Rust (with the `wasm32-unknown-unknown` target),
[`wasm-tools`], Node 22+ (for the Node host). The Node host needs a Node build
with JSPI (`--experimental-wasm-jspi`).

[`wasm-tools`]: https://github.com/bytecodealliance/wasm-tools

### Node (browser-first) host

```sh
cd hosts/node
npm install
npm run build:component   # build the guest .wasm component
npm run transpile         # jco transpile with JSPI async + host maps
node --experimental-wasm-jspi src/run.mjs
```

The host builds two `RTCPeerConnection`s with `@roamhq/wrtc`, performs a real
SDP/ICE handshake, and echoes on the far side.

### Wasmtime (native Rust) host

```sh
cd hosts/wasmtime
cargo run --release -- ../../components/echo-demo/build/echo-demo.component.wasm 1000 4096
#                                                                    ^msg count ^msg size
```

(Run the Node `build:component` step once first, or build the component
manually, to produce the `.wasm`.)

## Findings

**Feasibility: confirmed.** Both hosts run the identical component and
round-trip every message through a genuine WebRTC/SCTP data channel:

| Host | Stack | 1000 × 4096-byte round trip |
| --- | --- | --- |
| Node | `jco` (JSPI) + `@roamhq/wrtc` | ✅ correct, ~0.2 MiB/s |
| Wasmtime | Wasmtime 46 async + `webrtc-rs` | ✅ correct, ~10 MiB/s |

Notes and caveats (this is a spike):

- **The Component Model async ABI is a good fit for data channels.** Modeling
  each direction as `stream<list<u8>>` maps cleanly onto WebRTC's
  message-oriented channels and onto both `ReadableStream` (browser/`jco`) and
  `futures::Stream` (Rust). Backpressure and pipelining fall out of the ABI.
- **The Node/`jco` path is correct but slow.** Throughput is dominated by
  per-message JSPI stack-switch overhead, not by WebRTC. It is fine for
  development/testing (the browser-first goal) but is not the performance
  story; the native host is. Optimizing the JS path (e.g. batching across the
  JSPI boundary) is future work.
- **`jco` delivers a `send` stream argument as an async-iterable**, while
  `receive` returns a `ReadableStream`; the host code accommodates both.
- **The Wasmtime host uses the current (Wasmtime 46) component-model async host
  API** — `StreamReader`/`StreamProducer`/`StreamConsumer` with the `Accessor`
  concurrency model. This API is young; the `pipe.rs` adapters are adapted from
  Wasmtime's own test utilities.
- **`signaling` is designed but not yet exercised.** The demo uses the
  `connect` shortcut. A natural next step is a guest that drives the full
  `peer-connection` signaling interface against a real remote peer, exchanging
  SDP/ICE through the demo `rendezvous` mailbox over `wasi:http@0.3` and an
  existing signaling server (see [`AGENTS.md`](AGENTS.md)).

## Layout

```
wit/                            # the interface (single source of truth)
  webrtc-echo-demo.wit          #   demo:webrtc-echo package (connect, rendezvous, demo, world)
  deps/webrtc-data-channels/
    webrtc.wit                  #   wasi:webrtc-data-channels package (types, data-channels, signaling)
components/echo-demo/            # example guest component (Rust)
  wit -> ../../wit              #   symlink to the canonical wit/
hosts/node/                      # browser-first host (Node + jco + @roamhq/wrtc)
hosts/wasmtime/                  # native host (Wasmtime + webrtc-rs)
  wit -> ../../wit              #   symlink to the canonical wit/
```

The WIT is no longer duplicated: the component and the Wasmtime host reach the
canonical [`wit/`](wit) tree through `wit` symlinks, so there is a single copy
to edit. (The Node host reads the WIT embedded in the built component, so it
needs no `wit/` of its own.)
