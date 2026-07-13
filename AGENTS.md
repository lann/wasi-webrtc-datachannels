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
  webrtc.wit                           #   types (structural), connections (resources)
wasmtime-impl/                         # Wasmtime host crate (webrtc-rs),
                                       #   modeled after wasmtime_wasi_http::p3;
                                       #   add_to_linker + WasiWebrtcView (types + connections.data-channel);
                                       #   crate name: wasmtime-webrtc-datachannels
jco-impl/                              # browser-first host (Node + jco + @roamhq/wrtc)
wasip3-impl/                           # sans-I/O host crate on the wasm-capable
                                       #   lann/rtc `wasi` fork + native UDP driver;
                                       #   proves rtc<->webrtc-rs interop (DTLS+SCTP);
                                       #   crate name: wasip3-webrtc-datachannels
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
                                       #   the shared types/connections host lives in
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

- **`lann:webrtc-datachannels`** (`wit/webrtc.wit`) — the shared interfaces,
  split by ownership: `types` holds every structural (non-resource) type, while
  `connections` holds the two stateful resources — the `data-channel` transport
  and the `RTCPeerConnection`-style `peer-connection` design target. Structural
  types can be shared across a composition; the resources are each owned by the
  one component that implements them.
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
# Guest component (produces examples/echo-demo/build/echo-demo.component.wasm):
just build-component

# Node (browser-first) host:
just demo-node

# Browser host test (headless Chrome 137+; the same webrtc.js + component as the
# Node host, run through a real browser — this is the CI check for the browser
# path). Requires a Chrome/Chromium binary (auto-detected, or set CHROME_PATH):
just test-browser

# Wasmtime (native) host (defaults: 1000 messages of 4096 bytes):
just demo-wasmtime          # or: just demo-wasmtime 1000 4096

# Manual-signaling integration test (builds a guest, drives a real webrtc-rs
# manual-signaling round trip through a test-only host under wasmtime-impl/tests);
# it is part of `just test`:
just test
```

The recipes above are the underlying npm/cargo invocations documented in
[`README.md`](README.md); the [`justfile`](justfile) is the single entry point so
humans, agents, and CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml))
all run the same commands. Run `just` with no arguments to list every recipe.

### Checks to run before committing

Run the check recipes that cover what you changed **before committing**, and fix
anything they report. `just check` is the fast pre-commit gate; `just ci` mirrors
CI exactly (it additionally builds the guest component, transpiles it, and runs
the headless-browser test). Match the recipe to the change:

| Recipe | Run it when you change… |
| --- | --- |
| `just fmt-check` | any Rust source (formatting). |
| `just clippy` | any Rust source (lints all crates and both wasm targets). |
| `just validate-wit` | any `.wit` file (root `wit/` or a demo `examples/<name>/wit/`). |
| `just test` | any Rust host/guest code, or the manual-signaling test host. |
| `just build-component` | the `echo-demo` guest or its WIT. |
| `just transpile` | anything affecting the component's interfaces, or the `jco transpile` flags / `--map` targets in `jco-impl`. |
| `just test-browser` | the browser host (`jco-impl`, e.g. `webrtc.js`) or the component it runs. |
| `just check` | broad Rust/WIT changes — the quick gate for most commits. |
| `just ci` | anything touching the guest, jco host, or WIT — reproduces the full CI run locally. |

`just transpile` and `just test-browser` depend on `just build-component`, so
running either rebuilds the component first. Keep the two hosts producing the
same result.

## Code comments

Code comments describe **what** something is or does, not the process by which
it was arrived at.  Rationale such as "we removed X because Y" or "no bridge is
needed because…" belongs in commit messages, PR descriptions, or chat — not in
source files.  Keeping process reasoning out of comments avoids cluttering the
codebase with context that quickly becomes stale and misleading.

## Real signaling (`rendezvous` + `wasi:http@0.3`) — direction

The runnable demo uses the `connect` shortcut: the host builds *both* peers
internally, so no external signaling happens. To support genuinely separate
peers (developed and tested locally), two component instances — an offerer and
an answerer — must exchange SDP and trickled ICE out of band.

The intended shape:

- The guest drives the `lann:webrtc-datachannels/connections` `peer-connection`
  interface to produce/consume offers, answers, and ICE candidates.
- Those opaque blobs travel between the two peers through the demo-only
  `demo:webrtc-echo/rendezvous` mailbox interface. It is deliberately **not**
  standardized and lives in the demo package.
- A host implements `rendezvous` by relaying blobs to and from an **existing**
  HTTP signaling server over **`wasi:http@0.3`** (the guest never speaks HTTP
  itself). Because the whole loop is plain HTTP, the server can run locally.

`rendezvous` is defined but not yet wired into the `webrtc-echo-demo` world —
mirroring how `connections.peer-connection` is "designed but not yet exercised". Wiring it up
(host implementations for both stacks, a chosen signaling server, and a guest
that drives it) is the natural next step; see the starter's `wasi:http` example
for the client pattern.

## In-guest sans-I/O WebRTC (`wasip3-impl`) — direction

The two demo hosts run the fully async `webrtc-rs` engine host-side. To move the
WebRTC stack *into a wasm guest*, the protocol logic must be separated from I/O
so the guest can drive it over `wasi:sockets` and WASI timers. The sans-I/O
[`lann/rtc`](https://github.com/lann/rtc/tree/wasi) `wasi` fork makes that
possible: it compiles for `wasm32-wasip2` (the fork stubs `ifaces()` to return
`Unsupported` on wasm and bumps `rtc-mdns`'s `socket2` to 0.6). The `rtc`
dependency is pinned once at the workspace level in the root `Cargo.toml`.

[`wasip3-impl/`](wasip3-impl) is the first step down that path:

- `SansIoPeer` is the runtime-agnostic core — it wraps an `rtc`
  `RTCPeerConnection` and exposes signaling primitives plus the six sans-I/O
  stepping calls (`poll_transmit` / `handle_input` / `poll_timeout` /
  `handle_timeout` + drained events), performing no I/O itself.
- `NativePeer` is a **host-side** Tokio UDP reference driver. The loop lives
  host-side for now because that is what is CI-testable: `tests/interop.rs`
  stands up a `webrtc-rs` offerer and the sans-I/O answerer in one process and
  round-trips a data channel over real DTLS + SCTP.

Because the sans-I/O model has no OS interface enumeration, host candidates are
supplied explicitly by the driver (`add_local_host_candidate`) rather than
gathered from mDNS. The natural next step is a **guest** driver that feeds the
same `SansIoPeer` from `wasi:sockets`/timers instead of Tokio.
