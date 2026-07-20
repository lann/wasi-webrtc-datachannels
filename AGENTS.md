# AGENTS.md

Guidance for automated agents (and humans) working in this repository.

## What this repository is

`lann:webrtc-datachannels`: a WIT interface plus multiple implementations that
run the *same* guest component over a real WebRTC data channel: two hosts
(Wasmtime and jco) and one in-guest component (`wasip3-impl`). It is
intentionally small — prefer clarity and correctness over features, and keep the
implementations behaviourally in sync (asserted by the conformance suite under
[`conformance/`](conformance)). See [`README.md`](README.md) for the findings
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
                                       #   add_to_linker + WasiWebrtcView (types + connections.data-channel-options/data-channel);
                                       #   crate name: wasmtime-webrtc-datachannels
jco-impl/                              # browser-first host (Node + jco + @roamhq/wrtc)
wasip3-impl/                           # wasm COMPONENT on `rtc` 0.20: runs the
                                       #   sans-I/O stack in-guest (SansIoPeer core
                                       #   + a wasi:sockets/clocks runtime driver)
                                       #   and EXPORTS lann:webrtc-datachannels/
                                       #   connections; composable via `wac plug`;
                                       #   crate: wasip3-webrtc-datachannels
examples/                              # guest components + the demo/manual-signaling driver
  echo-demo/                           # example guest component (Rust)
    wit/                               #   demo-only WIT for this component
      webrtc-echo-demo.wit             #     demo:webrtc-echo (connect, rendezvous, demo)
      deps/lann-webrtc-datachannels -> ../../../../wit   # symlink to the root package
  cli-signaling/                       # manual-signaling CLI guest component (Rust)
    wit/                               #   demo-only WIT for this component
      webrtc-echo-demo.wit             #     demo:webrtc-echo (manual-signaling + world)
      deps/lann-webrtc-datachannels -> ../../../../wit   # symlink to the root package
  webrtc-consumer/                     # minimal consumer that IMPORTS connections;
                                       #   composed (`wac plug`) with wasip3-impl for
                                       #   the in-guest round-trip integration test
    wit/deps/lann-webrtc-datachannels -> ../../../../wit  # symlink to the root package
  wasmtime-demo/                       # native host (Wasmtime + webrtc-rs): demo binaries
                                       #   + the demo-only manual-signaling host
                                       #   (src/manual.rs, also reused by the
                                       #   wasmtime-impl integration test); the shared
                                       #   types/connections host lives in wasmtime-impl
conformance/                           # cross-implementation conformance suite
  guest/                               #   the shared conformance guest component
  adapters/                            #   per-target drivers: wasmtime, jco (Node +
                                       #     browser), wasip3 (composed in-guest stack),
                                       #     plus the interop-pair and ICE-lab binaries;
                                       #     common/ = the shared native building blocks
                                       #     (conformance-adapter-common) + the
                                       #     target-neutral Shadow-lab executor
  runner/                              #   classifies results against manifests and
                                       #     renders conformance/matrix.md
  signaling/                           #   conformance-signalingd HTTP mailbox server
  scenarios/                           #   netns/coturn scripts for the ICE lab
  manifests/, tests.toml               #   per-target expectations + the test corpus
scripts/setup.sh                       # one-shot dependency setup (see below)
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
  `connections` holds the stateful resources — the `data-channel-options`
  configuration builder, the `data-channel` transport, and the
  `RTCPeerConnection`-style `peer-connection` design target. Structural
  types can be shared across a composition; the resources are each owned by the
  one component that implements them.
- **`demo:webrtc-echo`** — the demo-only interfaces, split across the demo
  components that use them:
  - `examples/echo-demo/wit/webrtc-echo-demo.wit` — `connect`, `rendezvous`,
    `demo`, and the `webrtc-echo-demo` world.
  - `examples/cli-signaling/wit/webrtc-echo-demo.wit` — the vanilla
    `manual-signaling` surface and the `manual-signaling-host` world.

Cross-package `use` must include the version, e.g.
`use lann:webrtc-datachannels/types@0.1.0.{error}`.

Changing an interface identifier (package, interface, or function name) means
updating the consumers that name them as strings:

- the guest bindings in `examples/echo-demo/src/lib.rs` and
  `examples/cli-signaling/src/lib.rs`,
- the host bindings in
  `wasmtime-impl/src/bindings.rs` (whose
  `wit/world.wit` also pulls in the root package through a
  `deps/lann-webrtc-datachannels` symlink),
- the Wasmtime host bindings in `examples/wasmtime-demo/src/main.rs`,
  `examples/wasmtime-demo/src/manual.rs` (the manual-signaling host, also
  reused by `wasmtime-impl/tests/manual_signaling.rs`), and
  `examples/wasmtime-demo/src/bin/cli-signaling.rs`,
- the conformance guest, adapters, and jco transpile flags under
  `conformance/`, and
- the `jco transpile` `--async-exports` / `--async-imports` / `--map` flags in
  `jco-impl/package.json`.

## Build & run

Prerequisites: Rust with the `wasm32-unknown-unknown` target, `wasm-tools`, and
Node 24+ for the Node paths (jco's async ABI uses JSPI, which Node exposes on
24+ behind `--experimental-wasm-jspi`).

### One-shot dependency setup: `scripts/setup.sh`

[`scripts/setup.sh`](scripts/setup.sh) installs everything the build steps below
need and is the single source of truth shared by local developers, CI
([`.github/workflows/ci.yml`](.github/workflows/ci.yml)), and the Copilot cloud
agent ([`.github/workflows/copilot-setup-steps.yml`](.github/workflows/copilot-setup-steps.yml)).
It is idempotent, so it is safe to re-run. Assuming a Rust toolchain (via
`rustup`) and Node 24+ are already present, run it once from the repository root:

```sh
./scripts/setup.sh
```

It adds the `wasm32-unknown-unknown` and `wasm32-wasip2` Rust targets; installs
`wasm-tools`, `just`, `cargo-nextest`, `wac`, and `wasmtime` (each skipped if
already on `PATH`; versions pinned via `*_VERSION` variables); installs the
ICE-lab tools (iproute2, nftables, coturn; skip with `SKIP_ICE_LAB=1`); and runs
`npm install` in `jco-impl` and `conformance/adapters/jco`. Set `SKIP_NODE=1` to
skip the Node dependencies when you only need the Rust/Wasmtime path. It does
**not** install the Shadow network simulator (see below). CI is kept in sync by
calling this same script rather than duplicating the install steps.

Shadow ships no upstream prebuilt binary and is slow to build, so it is built
once by the `shadow-build` workflow (`.github/workflows/shadow-build.yml`, a
`workflow_dispatch`-only job that runs `scripts/build-shadow.sh`) and published
to this repository's `shadow-dev` GitHub prerelease. Install it into `~/.local`
either by downloading that binary (`./scripts/download-shadow.sh`) or by building
it locally (`./scripts/build-shadow.sh`); CI's Shadow-lab job and
`copilot-setup-steps.yml` download it from the release. The `just
conformance-shadow` recipe prints this guidance and fails if the binary is
missing when the lab runs.

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

# In-guest WASIp3 integration test: build the wasip3-impl provider component
# and the webrtc-consumer, compose them with `wac plug`, and run the single
# composed component under `wasmtime` — two peers connect over wasi:sockets UDP
# loopback entirely in-guest and exchange a message each way. Needs `wasmtime`
# (v46+) and `wac` on PATH; the recipe passes the async + WASIp3 flags:
just test-webrtc-composed

# Manual-signaling integration test (builds a guest, drives a real webrtc-rs
# manual-signaling round trip through the demo manual-signaling host);
# it is part of `just test`:
just test

# Cross-implementation conformance suite (loopback): builds the shared
# conformance guest, runs every enabled adapter (wasmtime, jco-node,
# jco-browser, wasip3-guest) plus the interop pairs, and renders
# conformance/matrix.md. Needs Node 24+ and a Chrome 137+ binary:
just conformance

# Conformance ICE lab (real non-loopback candidate paths via network
# namespaces; needs sudo and coturn — see the recipe comments):
just conformance-ice lan

# Conformance Shadow lab (the two-peer corpus for the wasmtime and
# wasip3-guest targets over a non-loopback path inside the Shadow
# discrete-event network simulator — deterministic, no root or
# network namespaces). Needs `shadow` on PATH (install with
# scripts/download-shadow.sh or scripts/build-shadow.sh):
just conformance-shadow
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
| `just clippy` | any Rust source (lints all crates and every wasm target, including `wasip3-webrtc-datachannels` and `webrtc-consumer`). |
| `just validate-wit` | any `.wit` file (root `wit/`, `wasip3-impl/wit/`, or a demo `examples/<name>/wit/`). |
| `just test` | any Rust host/guest code, or the manual-signaling test host. |
| `just build-component` | the `echo-demo` guest or its WIT. |
| `just test-webrtc-composed` | the `wasip3-impl` provider component, the `webrtc-consumer`, or the `connections` WIT (composes them with `wac plug` and runs the round trip under `wasmtime`). |
| `just transpile` | anything affecting the component's interfaces, or the `jco transpile` flags / `--map` targets in `jco-impl`. |
| `just test-browser` | the browser host (`jco-impl`, e.g. `webrtc.js`) or the component it runs. |
| `just conformance` | any host/guest behavior the suite asserts — the WIT surface, a host implementation, the conformance guest, adapters, or manifests (CI runs it in `.github/workflows/conformance.yml`). |
| `just check` | broad Rust/WIT changes — the quick gate for most commits. |
| `just ci` | anything touching the guest, jco host, or WIT — reproduces the full CI run locally. |

`just transpile` and `just test-browser` depend on `just build-component`, so
running either rebuilds the component first. Keep the implementations producing
the same result — the conformance suite is what asserts it.

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
`rtc` 0.20 stack makes that possible: it compiles for `wasm32-wasip2`
(`ifaces()` returns `Unsupported` on wasm). The `rtc`
dependency is pinned once at the workspace level in the root `Cargo.toml`.

[`wasip3-impl/`](wasip3-impl) is that component: a `cdylib` built for
`wasm32-wasip2` that imports only `wasi:sockets`/`wasi:clocks` and **exports**
`lann:webrtc-datachannels/connections`. It has one core and one driver:

- `SansIoPeer` (`src/peer.rs`) is the runtime-agnostic core — it wraps an `rtc`
  `RTCPeerConnection` and exposes signaling primitives plus the six sans-I/O
  stepping calls (`poll_transmit` / `handle_input` / `poll_timeout` /
  `handle_timeout` + drained events), performing no I/O itself.
- The `runtime` module (`src/runtime.rs`) is the **in-guest** driver: it feeds
  the `SansIoPeer` from WASIp3 `wasi:sockets` UDP and `wasi:clocks` timers,
  running the event loop as a detached task (`wit_bindgen::spawn`), since the
  component-model async model is single-threaded with no cross-thread `spawn`.
- The `provider` module (`src/provider.rs`) implements the exported
  `connections` resources (`data-channel-options`, `data-channel`,
  `peer-connection`) on top of the driver.

Because it exports the package surface and imports only WASIp3 interfaces, it is
composable: [`examples/webrtc-consumer`](examples/webrtc-consumer) imports
`connections` and is composed with the provider via `wac plug`, then run under
`wasmtime` (`just test-webrtc-composed`) — two peers connect over `wasi:sockets`
UDP loopback entirely in-guest and exchange a message each way.

Because the sans-I/O model has no OS interface enumeration, each
`peer-connection` supplies its own host candidate explicitly
(`add_local_host_candidate`) from the socket it binds, rather than gathering from
mDNS. `peer-connection` binds the IP address named by the `WEBRTC_UDP_BIND_ADDR`
environment variable, defaulting to IPv4 loopback (same-host peers); a routable
address gives the peer a host candidate reachable across a real network path,
as the conformance Shadow lab exercises. The remaining step is pairing that
with the `rendezvous` signaling above, so two separate components can connect
across a real deployment.
