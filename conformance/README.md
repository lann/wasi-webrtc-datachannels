# Conformance test suite

A conformance suite for the `lann:webrtc-datachannels` implementations. It runs
the **same wasm conformance guest component** against each target and asserts
**WIT-observable interoperable behavior only** — never SDP contents, candidate
ordering, timing, or exact error strings.

See [`PLAN.md`](PLAN.md) for the full design and the phased implementation plan.
This suite is being built one phase at a time; the sections below describe what
exists today.

## Status

Phases in place so far:

- **Phase 0 (scaffolding & registry):** the test registry, the manifest schema,
  and the `conformance-runner` that aggregates adapter results, applies
  expected-fail policy, and renders the matrix.
- **Phase 1 (signaling server):** `conformance-signalingd`, the suite-owned HTTP
  mailbox that relays opaque SDP/ICE blobs between two peers. The runner starts
  it (ephemeral localhost port, gated on `/healthz`) and tears it down.
- **Phase 2 (conformance guest + wasmtime adapter):** the shared conformance
  guest component (`guest/`, exporting `conformance:suite/runner`) and the
  `wasmtime` adapter (`adapters/wasmtime/`), which runs that guest against the
  native Wasmtime host (loopback ICE, in-process signaling) and emits the
  adapter result document the runner classifies against
  `manifests/wasmtime.toml`.
- **Phase 3 (jco adapters + first interop pair):** the browser-first host
  transpiled by jco (`adapters/jco/`), run two ways — under Node with
  `@roamhq/wrtc` (`jco-node`, `run-node.mjs`) and inside headless Chromium via
  playwright-core (`jco-browser`, `run-browser.mjs`) — plus the cross-runtime
  interop pair `wasmtime`<->`jco-node` in both orders (the `conformance-interop`
  binary in `adapters/wasmtime/`, which drives one wasmtime peer via the adapter
  library and the jco-node peer via `run-node.mjs --interop`). The jco host
  drives real signaling over the suite mailbox with `fetch` (`signaling.js`) and
  implements the full `connections` surface (`webrtc.js`); the shared corpus
  orchestration lives in `driver.js`. Classified against `manifests/jco-node.toml`,
  `manifests/jco-browser.toml`, `manifests/wasmtime-x-jco-node.toml`, and
  `manifests/jco-node-x-wasmtime.toml`.

  jco's async ABI always uses JSPI (`WebAssembly.Suspending`), so the Node-driven
  targets (`jco-node` and the jco-node half of the interop pair) require **Node
  24+** run with `--experimental-wasm-jspi`; the `jco-browser` target runs the
  guest in headless Chrome, which has JSPI natively.
- **Phase 4 (wasip3-guest adapter + second interop pair):** the whole WebRTC
  stack in wasm (`adapters/wasip3/`): the shared conformance guest is composed
  (`wac plug`) with the `wasip3-impl` provider component (the sans-I/O `rtc`
  stack driven over WASIp3 `wasi:sockets` UDP), an in-guest `wasi:http` mailbox
  client (`mailbox/`), and a CLI driver exporting an async `wasi:cli/run`
  (`driver/`) that reports each test's single-line JSON result on stdout. The
  `conformance-adapter-wasip3` orchestrator runs the composed component under
  `wasmtime run` (v46+; component-model async + WASIp3 + `wasi:http`) — one
  process per peer for the two-peer behavioral tests, connecting over
  `wasi:sockets` UDP loopback across processes and signaling through the suite
  mailbox — and writes `results/wasip3-guest.json`. The `conformance-interop`
  binary drives the `wasmtime`<->`wasip3-guest` pair in both orders the same
  way it drives the jco-node pair (currently disabled by default in the
  `conformance-interop` recipe — see TODO.md item E3). Classified against
  `manifests/wasip3-guest.toml`, `manifests/wasmtime-x-wasip3-guest.toml`, and
  `manifests/wasip3-guest-x-wasmtime.toml`.

## Running

From the repository root:

```sh
just conformance
```

This builds the conformance guest component and `conformance-signalingd`, runs
the `wasmtime` adapter (which starts its own in-process signaling server and
writes `conformance/results/wasmtime.json`), transpiles the guest for the jco
adapters and runs the `jco-node` and `jco-browser` targets, composes and runs
the `wasip3-guest` target under `wasmtime run`, runs the enabled interop pairs
(`wasmtime`<->`jco-node`, both orders; the `wasmtime`<->`wasip3-guest` pairs
are wired but disabled by default — see TODO.md item E3), then invokes
`conformance-runner`, which reads the test registry, the per-target manifests,
and those adapter result documents, starts and health-checks a standalone
signaling server, applies the expected-fail / unexpected-pass policy, tears the
server down, and writes the markdown matrix to `conformance/matrix.md`. It exits
nonzero on any `fail` or `unexpected-pass`.

The jco targets and their interop pair invoke `node --experimental-wasm-jspi`, so
a JSPI-capable **Node 24+** must be on `PATH` (see the Phase 3 note above). The
`conformance-interop` binary honours the `CONFORMANCE_NODE` environment variable
if a specific node binary is needed. The `wasip3-guest` target and its interop
pair invoke `wasmtime run` (v46+, installed by `scripts/setup.sh`; overridable
via `CONFORMANCE_WASMTIME`). To run a single target in isolation, use the
per-target recipes (`just conformance-jco-node`, `just conformance-jco-browser`,
`just conformance-wasip3`, `just conformance-interop`).

Within each adapter, tests run **in parallel by default** (4 at a time): every
test's peers use fresh guest instances (or processes) and their own signaling
room, so tests are independent. Each adapter exposes a `--jobs` flag to change
the concurrency (`--jobs 1` restores serial execution). Every connection
attempt is individually bounded (45s, retried with a fresh room), and the
`just` recipes additionally cap each whole adapter run (`conformance-timeout`,
600s by default) so a systemic hang fails in minutes rather than stalling CI.

Run the runner's unit tests and the signaling server's integration tests with
the rest of the workspace:

```sh
just test
# or just the signaling server:
cargo nextest run -p conformance-signalingd
```

## Signaling server (`conformance-signalingd`)

A small standalone HTTP mailbox server used to relay signaling blobs between two
peers over plain HTTP/1.1 long-poll (no WebSockets), reachable identically from
native Rust, browser/Node `fetch`, and `wasi:http`. The full wire contract is in
[`signaling/PROTOCOL.md`](signaling/PROTOCOL.md).

Run it standalone (binds an ephemeral localhost port and prints its URL):

```sh
cargo run -p conformance-signalingd
# or bind a fixed routable address for cross-machine / NAT-lab runs:
cargo run -p conformance-signalingd -- --host 0.0.0.0 --port 8080
```

## Layout

```
conformance/
  PLAN.md                  # design + phased plan
  README.md                # this file
  tests.toml               # test registry: id, tags, description
  manifests/               # per-target capability manifests (<target>.toml)
    example.toml.example   #   template (NOT loaded; see below)
  runner/                  # conformance-runner (Rust workspace member)
  signaling/
    PROTOCOL.md            # mailbox wire protocol spec
    server/                # conformance-signalingd (Rust workspace member)
  wit/                     # conformance WIT (Phase 2); deps symlink to root wit/
  guest/                   # conformance guest component(s) (Phase 2)
  adapters/                # per-target adapters (wasmtime / jco / wasip3)
  scenarios/               # ICE lab provisioning (Phase 5+)
```

## Test registry (`tests.toml`)

Every conformance test is declared once in [`tests.toml`](tests.toml) with a
stable `id`, a set of `tags`, and a one-line `description`. The conformance
guest mirrors these ids/tags via its `list-tests` export (Phase 2). See the
comments at the top of the file for the schema. Grow the corpus by adding
`[[test]]` entries; keep tags stable across growth so manifests remain valid.

## Capability manifests (`manifests/<target>.toml`)

Each target gets one manifest declaring:

- `[[unsupported]]` entries referencing **tags** — every matching test is
  reported `skip-unsupported` (visible in the matrix, never a failure). A
  mandatory `reason` explains why.
- `[[expected-fail]]` entries referencing **test ids** — a known divergence that
  keeps the run green while staying visible. A mandatory `tracking` reference
  (e.g. a `TODO.md` item) records the follow-up. An expected-fail that
  **passes** becomes `unexpected-pass` and **fails** the run, forcing the
  manifest to be cleaned up.

See [`manifests/example.toml.example`](manifests/example.toml.example) for the
full schema. That template has a `.toml.example` extension so the runner (which
loads only `*.toml`) does not treat it as an enabled target.

## Reading the matrix

`conformance-runner` renders a markdown table with one row per target and one
column per test. Cell values:

| Symbol | Meaning |
| --- | --- |
| `pass` | Passed and expected to pass. |
| `FAIL` | Failed and not expected to — **fails the run**. |
| `skip` | Skipped: the target's manifest declares the test's tag unsupported. |
| `xfail` | Expected-fail: failed as the manifest predicted (does not fail the run). |
| `UNEXPECTED-PASS` | An expected-fail that passed — **fails the run**; update the manifest. |
| `—` | No adapter reported a result for this (target, test). |

The runner exits nonzero if any cell is `FAIL` or `UNEXPECTED-PASS`.

## Adding a target

A new target = a new adapter (under `adapters/`) that emits a JSON result
document plus a new manifest (under `manifests/`). No change to the runner or
the registry is required. Result document shape:

```json
{ "target": "wasmtime", "environment": "loopback",
  "results": [ { "test_id": "ordering", "status": "pass", "detail": null } ] }
```

Adapters report raw `pass` / `fail` / `skip`; the runner applies manifest policy
and reclassifies.
