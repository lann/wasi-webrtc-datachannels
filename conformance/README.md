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

## Layering: adapters vs. environment executors

The suite separates *what runs a peer* from *where its network lives*:

- **Adapters** are environment-agnostic. Each target has one way to run a
  single guest instance of one test, configured entirely through
  flags/environment (test id, role, signaling URL, room, message parameters,
  bind address, ICE servers): the in-process adapters
  (`conformance-adapter-wasmtime`, the jco drivers,
  `conformance-adapter-wasip3`) and the per-process peers — the native
  `conformance-peer` binary and the composed wasip3 component under
  `wasmtime run`. A peer knows nothing about namespaces, simulators, or how its
  addresses were provisioned; every out-of-process peer honours the same
  single-peer contract (`--test`/`--role`/`--server`/`--room`/…, one JSON
  `test-result` line on stdout).
- **Environment executors** own the network environment and drive the corpus
  through those peers: `conformance-netns` (in `adapters/common`) provisions
  the netns lab and places each peer with `ip netns exec`;
  `conformance-shadow` (also in `adapters/common`) renders a Shadow simulation
  config and lets the simulator launch the peers. Executors decide addresses,
  scenarios, and process placement, and pass everything a peer needs as
  configuration; both build each peer's command line from the shared
  per-target templates in `conformance-adapter-common`'s `peer_command`
  module.

Supporting a peer in a new environment therefore means teaching the executor a
per-role command template — not the peer about the environment.

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

## netns lab (`just conformance-netns`)

The default adapters connect their peers over the loopback interface. The **ICE
lab** (Phase 5) instead runs the two peers of each test over a real routed
network path, so the ICE handshake exercises non-loopback candidates — and, for
the server-mediated scenarios, is forced through a STUN/TURN server. It is a
small routed network of Linux **network namespaces** provisioned entirely with
`ip`, `nft`, and coturn's `turnserver` (no containers): an offerer, an answerer,
and a signaling namespace, each on its own `/30` subnet behind a router
namespace. The topology (namespace names, addresses, ports, TURN credentials)
and its provisioning live in Rust, in `conformance-adapter-common`'s `lab`
module (see its docs for the topology diagram and addresses).

Because the two peers live in separate namespaces, each runs as its own process,
placed with `ip netns exec`; the target-neutral environment executor
(`conformance-netns`, in `adapters/common`) provisions the lab, runs the signaling
server (and coturn) in the signaling namespace, drives the corpus, and tears the
lab down. Like the Shadow lab, its `--peer-kind` flag selects the peer: the
native `conformance-peer` binary (`wasmtime`, the default) or the composed
wasip3 conformance component under `wasmtime run` (`wasip3-guest`, whose
in-guest sans-I/O stack supports no STUN/TURN — only the `lan` scenario fits).
Results are written to `conformance/results/<target>-<scenario>.json` with the
scenario as the report's `environment`, so each scenario is its own matrix
row.

Scenarios:

| Scenario | What it exercises |
| --- | --- |
| `lan` | Direct host-candidate connectivity over the router (no server). |
| `stun-srflx` | coturn as a STUN server behind a port-restricted (cone) NAT; the router blocks the direct peer↔peer path so a server-reflexive path must be used, and the cone NAT lets it connect. |
| `turn-relay` | coturn as a TURN server; the direct path is blocked and the peers are relay-only, so data is relayed by coturn. |
| `nat-symmetric` | coturn as a STUN/TURN server behind a symmetric NAT; the direct path is blocked and the symmetric NAT makes srflx unusable, so ICE falls back to a TURN relay (Phase 6). |

Run a scenario from the repository root (requires **root**, for `ip netns
exec`, and `turnserver` on `PATH` for the non-`lan` scenarios — both provided by
[`scripts/setup.sh`](../scripts/setup.sh)):

```sh
just conformance-netns lan
just conformance-netns stun-srflx
just conformance-netns turn-relay
just conformance-netns nat-symmetric
# or run both NAT scenarios (the Phase 6 matrix) at once:
just conformance-nat
# or run the lan scenario with the wasip3-guest peer:
just conformance-netns lan wasip3-guest
```

For interactive debugging, a provisioned lab can be kept up across runs: run the
executor once, then re-run it with `--no-provision` (it neither provisions nor
tears down) while inspecting the namespaces with `ip netns exec` by hand.

### NAT matrix (Phase 6)

Server-reflexive candidates are only meaningful when a peer's mapped address
differs from its host address, which requires NAT between the peers and the
router. The NAT scenarios add an nftables source-NAT on the router (applied by the
`lab` module's nftables provisioning) that rewrites each peer's forwarded
traffic to its own "public" address:

- `stun-srflx` uses a **port-restricted (cone) NAT** (`snat … persistent`): the
  mapping is endpoint-independent, so the two peers can hole-punch their srflx
  candidates and connect — the meaningful server-reflexive path.
- `nat-symmetric` uses a **symmetric NAT** (`snat … random`): the mapping is
  endpoint-dependent, so the address the STUN server observed is useless to the
  peer and ICE must fall back to a TURN relay.

The NAT scenarios are part of the workstation-only netns lab (see below); run
them with `just conformance-nat`.

The netns lab is **workstation-only**: CI does not run it. CI's non-loopback
coverage comes from the Shadow lab (`shadow-lab` in
[`.github/workflows/conformance.yml`](../.github/workflows/conformance.yml)),
which needs no root or network namespaces; the netns lab remains the
higher-fidelity environment for exercising the STUN/TURN/NAT candidate paths
on a real kernel.

## Shadow lab (`just conformance-shadow`)

The **Shadow lab** gives the same "two peers on separate hosts over a
non-loopback path" property as the netns lab, but runs the peers inside the
[Shadow](https://github.com/shadow/shadow) discrete-event network simulator
instead of network namespaces. Shadow runs the *unmodified* peer and
`conformance-signalingd` binaries under a single deterministic simulation,
intercepting their syscalls to model the network in user space — so it needs
**no root and no network namespaces**, which makes it reproducible in
sandboxes and hosted CI where the netns lab cannot run.

A target-neutral environment executor (`conformance-shadow`, in
`adapters/common`) generates a Shadow YAML config with three hosts per test (a
signaling server, an offerer, and an answerer, each on its own simulated IP),
runs `shadow` once over the whole corpus, parses the per-host process stdout,
folds the two peer results, and writes
`conformance/results/<target>-shadow.json` (environment `shadow`), so each
target appears as its own matrix row. Its `--peer-kind` flag selects the
per-role peer command template:

- `wasmtime` runs the native `conformance-peer` binary, gathering each peer's
  host candidate from its simulated interface address (`--bind-addr`);
- `wasip3-guest` runs the fully composed wasip3 conformance component under
  `wasmtime run` (the same invocation as the loopback adapter), pointing the
  in-guest provider at each host's simulated address through the
  `WEBRTC_UDP_BIND_ADDR` environment variable.

Run both targets from the repository root (needs `shadow` on `PATH`). Shadow
ships no upstream prebuilt binary, so install it into `~/.local` by downloading
the prebuilt binary from this repository's `shadow-dev` GitHub prerelease
([`scripts/download-shadow.sh`](../scripts/download-shadow.sh)) or by building it
from source ([`scripts/build-shadow.sh`](../scripts/build-shadow.sh)).
`scripts/setup.sh` does not install Shadow; the recipe below prints this guidance
and fails if the binary is missing:

```sh
just conformance-shadow
```

Shadow does not implement UDP `SO_REUSEADDR`/`SO_REUSEPORT`, which webrtc's mDNS
multicast socket sets, so the `wasmtime` peers run with `--disable-mdns` (they
connect over explicit host candidates, so mDNS is unused); the sans-I/O wasip3
stack has no mDNS at all. Only the Shadow peers pass the flag; the netns
lab, which runs on a real kernel, is unchanged.

CI runs the Shadow lab in a dedicated job (`shadow-lab` in
[`.github/workflows/conformance.yml`](../.github/workflows/conformance.yml));
Shadow ships no prebuilt binary, so the job downloads the prebuilt binary from
the `shadow-dev` prerelease (built on demand by the `shadow-build` workflow)
rather than rebuilding from source.

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
  adapters/                # per-target adapters (wasmtime / jco / wasip3);
                           #   common/ holds the shared native building blocks
                           #   (registry/plans, peer subprocess invocation,
                           #   retry loop, corpus runner, result document),
                           #   the netns-lab topology/provisioning (netns +
                           #   nftables + coturn, in Rust), and the
                           #   target-neutral netns-lab and Shadow-lab executors
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

Either kind of entry may carry an optional `environments = ["shadow", ...]`
list scoping it to specific environments (the same strings adapters report,
e.g. `loopback`, `lan`, `stun-srflx`, `turn-relay`, `nat-symmetric`, `shadow`).
An entry without `environments` applies to every environment (the original
behavior, so existing manifests are unchanged); a scoped entry applies only
when the report's environment is in the list, and takes precedence over an
unscoped entry for the same tag/test in its environments. An empty
`environments = []` list is a manifest error.

See [`manifests/example.toml.example`](manifests/example.toml.example) for the
full schema. That template has a `.toml.example` extension so the runner (which
loads only `*.toml`) does not treat it as an enabled target.

## Reading the matrix

`conformance-runner` renders a markdown table with one row per
`(target, environment)` and one column per test. Most targets run only in the
`loopback` environment; the netns lab (above) adds `lan` / `stun-srflx` /
`turn-relay` / `nat-symmetric` rows for the `wasmtime` target, and the Shadow
lab adds a `shadow` row for the `wasmtime` and `wasip3-guest` targets. Cell
values:

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
