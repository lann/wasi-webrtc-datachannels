# AGENTS.md

Guidance for automated agents (and humans) working in this repository.

## What this repository is

`lann:webrtc-datachannels`: a WIT interface plus two host implementations that
run the *same* guest component over a real WebRTC data channel. It is
intentionally small — prefer clarity and correctness over features, and keep the
two hosts behaviourally in sync. See [`README.md`](README.md) for the findings
and the big picture.

## Living knowledge base: `lann/wasm-component-starter`

Before designing a world, changing WIT, or touching the async/streaming plumbing,
consult **[`lann/wasm-component-starter`](https://github.com/lann/wasm-component-starter)**.
Treat it as a *living knowledge base* for this project — it is expected to evolve,
so re-read it rather than relying on a cached summary:

- **[`OUTLINE.md`](https://github.com/lann/wasm-component-starter/blob/main/OUTLINE.md)** —
  a high-density agent reference for the Component Model & WASI: canonical specs,
  the toolchain ecosystem (`wasmtime`, `wasm-tools`, `wac`, `wit-bindgen`, `jco`),
  Rust authoring targets, `wasmtime` host-provisioning flags (e.g. `-S http`,
  `-S p3`, `-W component-model-async=y`), and the WASI 0.2 → 0.3 shift
  (`wasi:io` pollables replaced by native async; `wasi:http` incoming/outgoing
  merged). Read it before designing an interface.
- **[`examples/`](https://github.com/lann/wasm-component-starter/tree/main/examples)** —
  runnable projects that demonstrate patterns this repo relies on: exporting an
  async `run`, async streaming imports/exports flowing guest → host → guest,
  returning a stream from an async export via `wit_bindgen::spawn`, mapping an
  import to a JS adapter with `jco --map`, and fetching URLs over async
  `wasi:http` with the `wasip3` crate. The `browser-tgz-maker` and
  `cli-metadata-printer` apps are the closest analogues to the work here.

When a task involves a capability not yet used in this repo (most notably
`wasi:http@0.3` signaling — see below), look for a matching pattern in the
starter's examples first.

## Repository layout

```
wit/                                   # lann:webrtc-datachannels package
  webrtc.wit                           #   types, data-channels, signaling
wasmtime-impl/                         # Wasmtime host crate (webrtc-rs),
                                       #   modeled after wasmtime_wasi_http::p3;
                                       #   add_to_linker + WasiWebrtcView (types + data-channels);
                                       #   crate name: wasmtime-webrtc-datachannels
jco-impl/                              # browser-first host (Node + jco + @roamhq/wrtc)
examples/                              # guest components + the demo/manual-signaling driver
  echo-demo/                           # example guest component (Rust)
    wit/                               #   demo-only WIT for this component
      webrtc-echo-demo.wit             #     demo:webrtc-echo (connect, rendezvous, demo)
      deps/lann-webrtc-datachannels -> ../../../../wit   # symlink to the root package
  cli-signaling/                       # manual-signaling CLI guest component (Rust)
    wit/                               #   demo-only WIT for this component
      webrtc-echo-demo.wit             #     demo:webrtc-echo (prompt, manual-demo,
                                       #       manual-signaling, worlds)
      deps/lann-webrtc-datachannels -> ../../../../wit   # symlink to the root package
  wasmtime-demo/                       # native host (Wasmtime + webrtc-rs): demo binaries;
                                       #   the shared types/data-channels host lives in
                                       #   wasmtime-impl above
```

### WIT is organized by ownership — one copy of the shared package

The **`lann:webrtc-datachannels`** package is defined exactly once, at the root
[`wit/`](wit). Each demo component owns its **demo-only** WIT under its own
`examples/<name>/wit/` and pulls the package in through a
`wit/deps/lann-webrtc-datachannels` **symlink** back to the root, so there is a
single copy of the shared surface to edit. Do **not** copy the root package into
a component or replace those `deps` symlinks with real directories.

The WIT is split into two packages, keeping the shared and demo-only surfaces
separate:

- **`lann:webrtc-datachannels`** (`wit/webrtc.wit`) — the shared interfaces:
  `types`, `data-channels`, and the `RTCPeerConnection`-style `signaling` design
  target.
- **`demo:webrtc-echo`** — the demo-only interfaces, split across the demo
  components that use them:
  - `examples/echo-demo/wit/webrtc-echo-demo.wit` — `connect`, `rendezvous`,
    `demo`, and the `webrtc-echo-demo` world.
  - `examples/cli-signaling/wit/webrtc-echo-demo.wit` — `prompt`,
    `manual-demo`, the vanilla `manual-signaling` surface, and the
    `browser-signaling-demo` / `manual-signaling-host` worlds.

Cross-package `use` must include the version, e.g.
`use lann:webrtc-datachannels/types@0.1.0.{error}`.

Changing an interface identifier (package, interface, or function name) means
updating the consumers that name them as strings:

- the guest bindings in `examples/echo-demo/src/lib.rs` and
  `examples/cli-signaling/src/lib.rs`,
- the host bindings in
  `wasmtime-impl/src/bindings.rs` (whose
  `wit/world.wit` also pulls in the root package through a
  `deps/lann-webrtc-datachannels` symlink), and the manual-signaling test host
  bindings in `wasmtime-impl/tests/manual_host.rs`,
- the Wasmtime host bindings in `examples/wasmtime-demo/src/main.rs` and
  `examples/wasmtime-demo/src/bin/cli-signaling.rs`, and
- the `jco transpile` `--async-exports` / `--async-imports` / `--map` flags in
  `jco-impl/package.json`.

## Build & run

Prerequisites: Rust with the `wasm32-unknown-unknown` target, `wasm-tools`, and
Node 22+ with JSPI (`--experimental-wasm-jspi`) for the Node host.

### One-shot dependency setup: `scripts/setup.sh`

[`scripts/setup.sh`](scripts/setup.sh) installs everything the build steps below
need and is the single source of truth shared by local developers, CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)), and the Copilot cloud
agent ([`.github/workflows/copilot-setup-steps.yml`](.github/workflows/copilot-setup-steps.yml)).
It is idempotent, so it is safe to re-run. Assuming a Rust toolchain (via
`rustup`) and Node 22+ are already present, run it once from the repository root:

```sh
./scripts/setup.sh
```

It adds the `wasm32-unknown-unknown` and `wasm32-wasip2` Rust targets, installs
`wasm-tools` (skipped if already on `PATH`; version pinned via
`WASM_TOOLS_VERSION`), and runs `npm install` in `jco-impl`. Set `SKIP_NODE=1`
to skip the Node dependencies when you only need the Rust/Wasmtime path. CI is
kept in sync by calling this same script rather than duplicating the install
steps.

```sh
# Guest component (produces build/echo-demo.component.wasm):
cd jco-impl && npm install && npm run build:component

# Node (browser-first) host:
npm run transpile && node --experimental-wasm-jspi src/run.mjs

# Browser host test (headless Chrome 137+; the same webrtc.js + component as the
# Node host, run through a real browser — this is the CI check for the browser
# path). Requires a Chrome/Chromium binary (auto-detected, or set CHROME_PATH):
npm run test:browser

# Wasmtime (native) host:
cd ../examples/wasmtime-demo && cargo run --release --bin wasmtime-webrtc-host -- \
  ../echo-demo/build/echo-demo.component.wasm 1000 4096

# Manual-signaling integration test (builds a guest, drives a real webrtc-rs
# manual-signaling round trip through a test-only host under wasmtime-impl/tests):
cargo test --manifest-path ../wasmtime-impl/Cargo.toml
```

Validate what you touch: `cargo build` the crate(s) you changed, `wasm-tools
component wit` on each wit dir you edited (the root `wit/` and/or the affected
`examples/<name>/wit/`) after WIT edits, and re-run the Node transpile when the
component's interfaces change. When you touch the browser host (`jco-impl`),
run `npm run test:browser`. When you touch the manual-signaling integration
test host under `wasmtime-impl/tests`, run
`cargo test --manifest-path wasmtime-impl/Cargo.toml`. Keep the two hosts
producing the same result.

## Real signaling (`rendezvous` + `wasi:http@0.3`) — direction

The runnable demo uses the `connect` shortcut: the host builds *both* peers
internally, so no external signaling happens. To support genuinely separate
peers (developed and tested locally), two component instances — an offerer and
an answerer — must exchange SDP and trickled ICE out of band.

The intended shape:

- The guest drives the `lann:webrtc-datachannels/signaling` `peer-connection`
  interface to produce/consume offers, answers, and ICE candidates.
- Those opaque blobs travel between the two peers through the demo-only
  `demo:webrtc-echo/rendezvous` mailbox interface. It is deliberately **not**
  standardized and lives in the demo package.
- A host implements `rendezvous` by relaying blobs to and from an **existing**
  HTTP signaling server over **`wasi:http@0.3`** (the guest never speaks HTTP
  itself). Because the whole loop is plain HTTP, the server can run locally.

`rendezvous` is defined but not yet wired into the `webrtc-echo-demo` world —
mirroring how `signaling` is "designed but not yet exercised". Wiring it up
(host implementations for both stacks, a chosen signaling server, and a guest
that drives it) is the natural next step; see the starter's `wasi:http` example
for the client pattern.
