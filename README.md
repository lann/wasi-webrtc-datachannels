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
| [`wit/`](wit) | The reusable streaming **WIT interface**, the `wasi:webrtc-data-channels@0.1.0` package. Each demo component keeps its own demo-only WIT and symlinks this package in as a dependency. |
| [`examples/echo-demo`](examples/echo-demo) | A **Rust example component** exercising a data channel entirely through streams. |
| [`wasmtime-impl`](wasmtime-impl) | The **reusable Wasmtime host crate** (webrtc-rs), modeled after `wasmtime_wasi_http::p3`. Provides `add_to_linker` + `WasiWebrtcView` for the reusable `types` + `data-channels`. (Crate name stays `wasmtime-wasi-webrtc-datachannels`.) |
| [`jco-impl`](jco-impl) | The **browser-first host** (Node stand-in for the browser, jco + @roamhq/wrtc). |
| [`examples/wasmtime-demo`](examples/wasmtime-demo) | The **native Rust host** (Wasmtime + webrtc-rs): binaries plus a lib carrying the demo-only manual-signaling host and the integration test, built on `wasmtime-impl`. |
| [`examples/cli-signaling`](examples/cli-signaling) | The **manual-signaling CLI guest component** (Rust). |
| [`AGENTS.md`](AGENTS.md) | Orientation for agents/contributors, linking the `lann/wasm-component-starter` knowledge base. |

The same `echo-demo.component.wasm` produced from `examples/echo-demo` is
loaded by **both** hosts. That is the core compatibility result of the spike.

## The interface

The reusable interface lives at the root [`wit/`](wit) as the
`wasi:webrtc-data-channels` package. Each demo component keeps its own demo-only
WIT alongside it and pulls the reusable package in as a `deps` symlink, so there
is still a single copy of the shared surface to edit:

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

**`demo:webrtc-echo`** — the demo-only interfaces, which now live with the demo
components that use them ([`examples/echo-demo/wit`](examples/echo-demo/wit)
for the echo demo, [`examples/cli-signaling/wit`](examples/cli-signaling/wit)
for the manual-signaling demos) rather than at the root:

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
cd jco-impl
npm install
npm run build:component   # build the guest .wasm component
npm run transpile         # jco transpile with JSPI async + host maps
node --experimental-wasm-jspi src/run.mjs
```

The host builds two `RTCPeerConnection`s with `@roamhq/wrtc`, performs a real
SDP/ICE handshake, and echoes on the far side.

The same `webrtc.js` host module also runs unchanged in a real browser: it now
resolves `RTCPeerConnection` from the browser global when present and only falls
back to `@roamhq/wrtc` under Node. A headless-Chrome test drives the *identical*
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
- **The browser-first host really does run in a browser — and in CI.** The same
  `webrtc.js` and the same transpiled component drive a genuine WebRTC data
  channel inside headless Chrome. Two headless-specific gotchas had to be
  solved: JSPI must be available (it is, by default, in Chrome 137+), and
  Chrome's `FilteringNetworkManager` silently *discards* all host ICE candidates
  until the page holds a media permission — so the loopback handshake never
  completes. The fix is to serve the page from `http://127.0.0.1` (a secure
  context), launch with fake media devices, grant microphone/camera, and call
  `getUserMedia` before opening the peer connection; only then do real host
  candidates flow. See [`jco-impl/test/browser.mjs`](jco-impl/test/browser.mjs).
- **`signaling` is designed but not yet exercised.** The demo uses the
  `connect` shortcut. A natural next step is a guest that drives the full
  `peer-connection` signaling interface against a real remote peer, exchanging
  SDP/ICE through the demo `rendezvous` mailbox over `wasi:http@0.3` and an
  existing signaling server (see [`AGENTS.md`](AGENTS.md)).

## Layout

```
wit/                            # reusable wasi:webrtc-data-channels package
  webrtc.wit                    #   types, data-channels, signaling
wasmtime-impl/                  # reusable Wasmtime host crate (webrtc-rs),
                                #   add_to_linker + WasiWebrtcView (types + data-channels)
                                #   (crate name: wasmtime-wasi-webrtc-datachannels)
jco-impl/                        # browser-first host (Node + jco + @roamhq/wrtc)
examples/
  echo-demo/                     # example guest component (Rust)
    wit/                         #   demo-only WIT for this component
      webrtc-echo-demo.wit       #     demo:webrtc-echo (connect, rendezvous, demo, world)
      deps/wasi-webrtc-data-channels -> ../../../../wit   # symlink to the root package
  cli-signaling/                 # manual-signaling CLI guest component (Rust)
    wit/                         #   demo-only WIT for this component
      webrtc-echo-demo.wit       #     demo:webrtc-echo (prompt, manual-demo, manual-signaling, worlds)
      deps/wasi-webrtc-data-channels -> ../../../../wit   # symlink to the root package
  wasmtime-demo/                 # native host (Wasmtime + webrtc-rs): lib (demo-only
                                 #   manual-signaling host + integration test) + binaries,
                                 #   built on wasmtime-impl
```

The reusable `wasi:webrtc-data-channels` package is defined once at the root
[`wit/`](wit); each demo component symlinks it in under
`wit/deps/wasi-webrtc-data-channels` and keeps its own demo-only WIT next to it.
The Node host reads the WIT embedded in the built component, so it needs no
`wit/` of its own; the Wasmtime host binaries bindgen the demo component wit
dirs directly, and delegate the reusable `types`/`data-channels` surface to
`wasmtime-impl` (the demo-only `manual-signaling`
host lives in `examples/wasmtime-demo` itself).
