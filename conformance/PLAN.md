# Conformance test suite — design and implementation plan

This document is the executable plan for building a conformance test suite for
the WebRTC data-channel implementations in this repository. It is written to be
executed by later agent sessions, one phase per session, with an expert human
reviewer between phases. Each phase is self-contained, verifiable, and ends
with a green `just conformance` run for the subset enabled so far.

## Purpose

The project's goal is to provide implementations of the
`lann:webrtc-datachannels` component-model interfaces for browsers, mobile
devices, and cloud environments that can execute the **same wasm component**
with **compatible and interoperable — but not necessarily identical —**
behavior. The conformance suite makes that promise testable:

- **Behavioral conformance:** one shared conformance guest component runs
  against each implementation and must observe the semantics the WIT
  documents.
- **Interop conformance:** pairs of implementations connect to each other over
  real signaling and must successfully establish connections and exchange
  data.
- Assertions target *observable interoperable behavior only* — never SDP
  contents, candidate ordering, timing, or exact error strings. Only WIT-level
  outcomes (variant tags, message payloads, ordering where `ordered: true`,
  resource lifecycle behavior) are asserted.

The suite must (mostly) run on GitHub Actions. Multi-ICE-scenario testing runs
in CI where feasible (Linux network namespaces, local coturn) and on a
workstation where not (browser-behind-NAT, the full NAT matrix until proven
stable).

**Note:** the suite is designed independently of `TODO.md`. In particular, it
does **not** wait for the repo's `rendezvous`/`wasi:http` signaling work; it
owns its own signaling mechanism (see below). Where the suite encounters known
divergences that TODO items track (e.g. error taxonomy), those become
`expected-fail` manifest entries referencing the TODO item.

## Conformance targets

| Target id | What it is | Where it lives today |
| --- | --- | --- |
| `wasmtime` | Native Wasmtime host over webrtc-rs | `wasmtime-impl/` (+ `examples/wasmtime-demo/`) |
| `jco-node` | Node host (jco transpile + `@roamhq/wrtc`) | `jco-impl/` |
| `jco-browser` | Headless Chrome running the same transpiled component + `webrtc.js` | `jco-impl/test/browser.mjs` pattern |
| `wasip3-guest` | In-guest sans-I/O stack: `wasip3-impl` is itself a component that exports `connections` (driven over `wasi:sockets`), composed with the conformance guest and run under `wasmtime` | `wasip3-impl/`, `examples/webrtc-consumer/` |

`jco-node` and `jco-browser` are separate targets: they share `webrtc.js` but
diverge in ICE behavior (Chrome requires the fake-media/localhost secure
context dance or it discards host candidates; see `jco-impl/test/browser.mjs`
and `.github/workflows/ci.yml`).

The manifest system (below) is how future targets (mobile, other clouds) get
added without changing the suite: a new target = a new adapter + a new
manifest.

## Directory layout (to be created)

```
conformance/
  PLAN.md                  # this file
  README.md                # how to run the suite (written in Phase 0)
  tests.toml               # test registry: id, tags, description
  manifests/               # per-target capability manifests
    wasmtime.toml
    jco-node.toml
    jco-browser.toml
    wasip3-guest.toml
  runner/                  # Rust crate: conformance-runner (workspace member)
  signaling/
    PROTOCOL.md            # mailbox protocol spec (Phase 1)
    server/                # Rust crate: conformance-signalingd (workspace member)
  wit/                     # conformance:signaling package + conformance guest worlds
    deps/lann-webrtc-datachannels -> ../../wit   # symlink, per repo convention
  guest/                   # Rust conformance guest component(s)
  adapters/
    wasmtime/              # Rust test-host adapter crate
    jco/                   # Node adapters (node + browser) incl. signaling.js
    wasip3/                # in-guest driver component
  scenarios/               # ICE lab provisioning (netns/coturn/nftables scripts)
```

Follow the existing repo conventions: the shared `lann:webrtc-datachannels`
package is pulled in via a `deps` **symlink** to the root `wit/` (never
copied); new Rust crates join the root virtual workspace unless they must be
standalone (wasm-target guests follow the `echo-demo`/`cli-signaling`
exclusion pattern in `justfile`/CI).

## Layer 1: conformance guest components (the test corpus)

Rust guest component(s) importing `lann:webrtc-datachannels/connections` (and,
for interop tests, `conformance:signaling/mailbox`). Every target runs the
**same wasm binary**.

Export shape (a `conformance:suite` WIT world):

```
list-tests: func() -> list<test-descriptor>;      // id + tags, mirrors tests.toml
run-test: async func(test-id: string, config: test-config) -> test-result;
```

`test-config` carries role (`offerer`/`answerer`/`both`), signaling server URL
and room, and scenario knobs (message counts/sizes, trickle policy).
`test-result` is `pass | fail(detail: string) | skipped(reason: string)`.

### Test corpus (initial; grow via `tests.toml`)

**Data-channel semantics** (tags: `data-channel`, plus specific tags)
- `label-round-trip` — negotiated label observed identically by both peers.
- `binary-message`, `text-message` — kind preserved end to end.
- `message-boundaries` — N distinct messages arrive as N messages.
- `zero-length-message` — empty binary and empty string messages.
- `large-message` — payloads near SCTP practical limits (parameterized).
- `ordering` — with `ordered: true`, indexed payloads arrive in order.
- `payload-integrity` — indexed + checksummed payloads verified byte-for-byte.
- `concurrent-send-receive` — pipelined concurrent send/receive completes.
- `send-via-stream`, `receive-via-stream` — streaming forms round-trip;
  `stream-message` kind/length invariants hold.
- `receive-via-stream-once` — second call and subsequent `receive` return
  `error.receiving-via-stream`; pending `receive`s resolve with it.
- `post-close-send` — send after close yields `error.closed` (tag: `errors`).
- `max-retransmits-accepted` — option accepted; channel still functions
  (tag: `unreliable-channels`).

**Error taxonomy** (tags: `errors`) — each WIT `error` variant produced where
the docs require: `invalid-signaling` on malformed SDP/candidate, `closed` on
closed-channel ops, `timed-out` on handshake timeout. These start as
`expected-fail` for hosts that collapse everything to `other(string)`.

**Peer-connection semantics** (tags: `peer-connection`) — offer/answer state
machine happy path, `create-data-channel` + `incoming-data-channels`,
`local-ice-candidates` streaming + end-of-candidates, `add-ice-candidate`,
`wait-connected`, `close` releases resources (no hang on subsequent ops),
invalid SDP → `invalid-signaling`. Targets that don't implement
`peer-connection` yet declare the tag unsupported in their manifest, so these
tests double as the implementation roadmap.

**Interop handshake** (tags: `interop`, `signaling`) — full mailbox-driven
offer/answer + trickle ICE between two component instances, then a
payload-integrity exchange.

## Layer 2: per-target adapters

Each adapter instantiates the conformance guest on its target, runs a list of
test IDs with a given `test-config`, and writes one JSON result document:

```json
{ "target": "wasmtime", "environment": "loopback",
  "results": [ { "test_id": "ordering", "status": "pass", "detail": null } ] }
```

Statuses: `pass`, `fail`, `skip-unsupported`, `expected-fail`,
`unexpected-pass`. The runner (not the adapters) applies manifest policy; the
adapters report raw pass/fail/skip and the runner reclassifies.

- **`adapters/wasmtime`** — Rust test-host crate modeled on
  `wasmtime-impl/tests/manual_host.rs` (bindgen against the conformance world,
  `add_to_linker` from `wasmtime-impl`, loopback via the existing
  `set_setting_engine_hook`). Also hosts the native mailbox implementation.
- **`adapters/jco`** — Node CLI reusing the `jco transpile` output and
  `jco-impl/webrtc.js` + a new `signaling.js` (fetch-based mailbox client)
  mapped via `--map`; a browser mode extending the `browser.mjs` pattern
  (headless Chrome 137+, fake-media permission, localhost secure context).
- **`adapters/wasip3`** — the conformance guest composed (`wac plug`) with the
  `wasip3-impl` provider component (which already exports `connections`, driven
  in-guest over `wasi:sockets`) plus an in-guest `wasi:http` mailbox client,
  run via `wasmtime run -W component-model-async=y -S cli -S p3 -S http
  -S inherit-network`, results emitted on stdout.

## Layer 3: runner + comparator

`conformance-runner` (Rust binary):

1. Reads `tests.toml` and `manifests/*.toml`; computes each target's test list
   (tags declared unsupported → planned as `skip-unsupported`).
2. Provisions the scenario (spawns `conformance-signalingd`, and in ICE-lab
   scenarios invokes `scenarios/` provisioning), then invokes the adapters.
3. Aggregates adapter JSON, applies `expected-fail` policy (an expected-fail
   that passes becomes `unexpected-pass` and **fails CI** so manifests stay
   honest), renders a markdown conformance matrix
   (target × scenario × test → status) as a CI artifact, and exits nonzero on
   any `fail` or `unexpected-pass`.

## Declarative capability manifests

`conformance/manifests/<target>.toml`:

```toml
[target]
id = "jco-browser"

[[unsupported]]
tag = "netns-scenarios"
reason = "Chrome cannot be isolated in a network namespace in CI; run the NAT matrix on a workstation (see README)."

[[expected-fail]]
test = "error-invalid-signaling"
reason = "Host collapses all failures into error.other"
tracking = "TODO.md item 16"
```

- `unsupported` entries reference **tags** (stable across corpus growth), each
  with a mandatory reason. Matching tests report `skip-unsupported` — visible
  in the matrix, never a failure.
- `expected-fail` entries reference **test ids** with a mandatory tracking
  reference. They keep CI green while keeping divergences visible; an
  unexpected pass fails CI to force manifest cleanup.

## Conformance signaling mechanism (`conformance:signaling`)

Suite-owned signaling, deliberately separate from the demo `rendezvous`
proposal and from any future standardized signaling interface, so it can
evolve with the tests and be discarded without API cost.

### Server: `conformance-signalingd`

A small standalone Rust binary (`conformance/signaling/server/`) implementing
an HTTP mailbox:

- **Model:** a *room* holds exactly two ordered per-role mailboxes
  (`offerer`, `answerer`). Peers publish opaque blobs to their own mailbox and
  consume the peer's mailbox in publish order.
- **Endpoints:**
  - `POST /rooms/{room}/{role}` — publish the next blob.
  - `GET /rooms/{room}/{peer_role}?seq={n}` — fetch blob *n*, long-polling
    until available. Sequence-numbered fetches make reads idempotent and
    retry-safe (needed for flaky-network scenarios and browser reloads).
  - `POST /rooms/{room}/{role}/done` — end-of-blobs marker; fetch past the
    last blob returns a distinguished "no more" response.
  - `DELETE /rooms/{room}` — cleanup.
  - `GET /healthz` — readiness for the runner.
- Plain HTTP/1.1 + long-poll only (**no WebSockets**): reachable identically
  from Rust, Node/browser `fetch`, and `wasi:http`.
- Binds an ephemeral localhost port by default (the runner spawns one per
  scenario and passes the URL to adapters); can bind a routable address for
  workstation cross-machine and NAT-lab runs.
- In-memory state, per-room TTL, request-size caps, no auth (test-only,
  loopback/namespace-scoped).
- In ICE-lab scenarios the server runs in a dedicated "signaling" namespace
  reachable from both peer namespaces even when the direct peer media path is
  blocked — signaling always works while the media path is constrained, which
  is exactly what TURN-forcing scenarios require.

The full protocol is specified in `conformance/signaling/PROTOCOL.md`
(Phase 1 deliverable).

### Guest-facing WIT: `conformance:signaling/mailbox`

Modeled on the demo `rendezvous` shape (see
`examples/echo-demo/wit/webrtc-echo-demo.wit`), but suite-owned:

```
enum role { offerer, answerer }
resource session {
    open: static async func(server: string, room: string, as-role: role) -> result<session, error>;
    send: async func(blob: list<u8>) -> result<_, error>;
    recv: async func() -> result<option<list<u8>>, error>;   // none => peer done
    done: async func() -> result<_, error>;
}
```

Blob payloads are opaque to the interface. The conformance guest encodes
`session-description` and `ice-candidate` values as JSON with an explicit
`end-of-candidates` message — one wire format across all targets, and trickle
vs. non-trickle vs. late-candidate scenarios become pure blob schedules.

### Per-target mailbox implementations

- **wasmtime:** native host implementation (reqwest/hyper) in
  `adapters/wasmtime`, wired via `add_to_linker`.
- **jco-node / jco-browser:** shared `signaling.js` using `fetch`, mapped via
  `jco transpile --map`. Works unchanged in Chrome (localhost is a secure
  context; long-poll fetch needs no permissions).
- **wasip3-guest:** implemented *in-guest* over `wasi:http@0.3` outgoing
  requests (see the `wasm-component-starter` wasip3 http-client example;
  `wasmtime -S http`). Fallback if long-poll over `wasi:http` proves
  troublesome: short-poll with retry — the protocol supports this without
  changes.

### How the suite uses signaling

- **Intra-target behavioral tests** run both roles through one mailbox (two
  instances of the same target) — real signaling from day one; no dependency
  on the demo `connect` shortcut.
- **Cross-implementation interop matrix:** for each ordered pair of targets,
  the runner starts a server, launches target A as offerer and target B as
  answerer in the same room, and runs the corpus subset both manifests
  support.
- **ICE scenarios** select candidate policies and blob schedules through the
  same mechanism.

## ICE scenario matrix

| Scenario | Description | CI? |
| --- | --- | --- |
| `loopback` | Host candidates over 127.0.0.1 (existing setting-engine hook / fake-media patterns) | Yes, all targets |
| `lan` | Two Linux netns joined by veth; host candidates only; signaling server reachable from both | Yes (Linux runners allow `sudo ip netns`) |
| `stun-srflx` | Local coturn (apt/Docker) as STUN; peers in separate netns, host candidates filtered → forces srflx | Yes |
| `turn-relay` | coturn as TURN; direct paths blocked with nftables → forces relay; signaling via signaling netns | Yes (wasmtime, jco-node, wasip3-guest) |
| `nat-matrix` | Port-restricted and symmetric NAT via nftables masquerade; asserts srflx works where expected, relay fallback where required | Workstation-first; nightly CI once stable |
| `trickle-variants` | Trickle vs. non-trickle (candidates embedded in SDP) vs. late-candidate arrival — pure mailbox blob schedules | Yes, all targets |

Browser target: `loopback`, `stun-srflx`, `turn-relay` (Chrome can talk to a
local coturn) in CI; netns/NAT scenarios are declared unsupported-in-CI in its
manifest with workstation instructions in `README.md`.

Workstation entry point: `just conformance-ice` runs the full scenario set
including `nat-matrix`.

## CI integration

New `.github/workflows/conformance.yml` mirrored by `just conformance`:

- **Job 1 (every PR):** build the conformance guest once; `loopback` +
  `trickle-variants` across all targets, including the pairwise interop matrix
  over the local signaling server; upload matrix artifact; fail on unexpected
  statuses.
- **Job 2 (every PR, Linux):** `lan` / `stun-srflx` / `turn-relay` for
  wasmtime, jco-node, wasip3-guest.
- **Job 3 (nightly / workflow_dispatch):** `nat-matrix` and soak variants;
  `continue-on-error` until proven stable.

Dependencies install through `scripts/setup.sh` (extended for coturn,
iproute2, nftables), keeping one installer for developers, CI, and agents.

## Phased implementation plan

One phase ≈ one agent session. Every phase ends with: green
`just conformance` for the enabled subset, `just check` clean, updated
manifests, an updated matrix artifact, and a reviewer checklist in the PR
description. Do not start a phase until the previous phase's PR is merged or
the reviewer directs otherwise.

### Phase 0 — Scaffolding & registry
- Create the `conformance/` layout above; `README.md` (how to run; matrix
  interpretation); `tests.toml` schema (id, tags, description) and manifest
  schema; `conformance-runner` skeleton that parses both, aggregates stub
  adapter JSON, renders the markdown matrix, and applies the
  expected-fail/unexpected-pass policy.
- Add `just conformance` (runner over the empty set) and the workflow file
  running Job 1 with no targets enabled.
- **Done when:** runner passes on an empty test set in CI; schemas documented.

### Phase 1 — Signaling server + protocol
- Write `conformance/signaling/PROTOCOL.md`; implement `conformance-signalingd`
  (suggested: axum or hyper; workspace member).
- Integration tests: publish/fetch ordering, long-poll wakeup, seq idempotence
  (refetch returns the same blob), done-markers, room TTL expiry, size caps,
  concurrent rooms.
- Runner gains spawn/teardown + `/healthz` gating.
- **Done when:** `cargo nextest run -p conformance-signalingd` green in CI;
  runner can start/stop a server.

### Phase 2 — Signaling WIT + conformance guest + wasmtime adapter
- Define `conformance:signaling/mailbox` and the conformance-suite world under
  `conformance/wit/` (deps symlink to root `wit/`; add to `just validate-wit`).
- Implement the conformance guest with the data-channel corpus and the
  mailbox-driven handshake (JSON blob encoding incl. `end-of-candidates`).
- Implement `adapters/wasmtime` (bindgen per `manual_host.rs` pattern; native
  mailbox host; loopback via `set_setting_engine_hook`; JSON results output).
- Wire wasmtime into CI Job 1 (intra-target, both roles over real signaling).
- Populate `manifests/wasmtime.toml` (expected-fails for error-taxonomy tests,
  tracking TODO item 5; `peer-connection` tag unsupported until the host
  implements the resource).
- **Done when:** wasmtime row of the matrix is green (with declared
  expected-fails) in CI.

### Phase 3 — jco adapters
- `adapters/jco`: `signaling.js` fetch mailbox client; transpile the
  conformance guest with the right `--async-*`/`--map` flags (keep flags next
  to the adapter, not in `jco-impl/package.json`); Node runner mode; browser
  runner mode per `browser.mjs` (fake media, localhost secure context).
- Manifests for both targets (browser: netns/NAT tags unsupported-in-CI;
  expected-fails for typed errors, tracking TODO item 16).
- Enable the first interop pair in the runner: wasmtime ↔ jco-node (both
  orders).
- **Done when:** three target rows + the first interop rows green in CI Job 1.

### Phase 4 — wasip3-guest adapter
- `adapters/wasip3`: the conformance guest composed (`wac plug`) with the
  `wasip3-impl` provider (which exports `connections`) + an in-guest
  `wasi:http` mailbox client; runner invokes `wasmtime run` with the async/p3
  flags; results over stdout.
- Manifest documents any partial surface (e.g. tags for features the in-guest
  provider cannot express yet; explicit host candidates only — `ifaces()` is
  `Unsupported` on wasm; loopback bind address only).
- Include in loopback scenarios and the interop matrix.
- **Done when:** wasip3-guest row green in CI Job 1 with a manifest the
  reviewer has signed off.

### Phase 5 — ICE lab
- `scenarios/`: netns/veth provisioning scripts (idempotent setup/teardown,
  usable standalone), coturn config + launch, host-candidate filtering and
  nftables direct-path blocking for `stun-srflx`/`turn-relay`; the
  signaling-namespace topology.
- Runner scenario selection; CI Job 2; `just conformance-ice`; workstation
  docs in `README.md`; extend `scripts/setup.sh`.
- **Done when:** `lan`, `stun-srflx`, `turn-relay` green in CI Job 2 for
  wasmtime + jco-node (wasip3-guest where its manifest allows).

### Phase 6 — NAT matrix
- nftables NAT simulations (port-restricted, symmetric); assertions that
  srflx connects where expected and relay fallback engages where required;
  nightly CI Job 3 with `continue-on-error`; stability hardening (retries,
  generous timeouts, diagnostic capture on failure).
- **Done when:** NAT matrix runs clean on a workstation and the nightly job is
  wired (stability graduation to blocking is a later, human decision).

### Phase 7 — Full interop matrix + reporting polish
- All supported ordered target pairs across all supported scenarios; matrix
  artifact covering target × scenario × test; optionally publish the matrix to
  a committed doc or PR comment.
- Audit manifests: retire `expected-fail` entries whose fixes landed.
- **Done when:** the complete matrix is generated in CI and `README.md`
  explains how to read and extend it.

## Session guardrails for executors

- Read `AGENTS.md` first; run the matching `just` check recipes before
  committing (`just check` minimum; `just ci` when touching guests/jco/WIT).
- Never copy the root `wit/` package; always use `deps` symlinks.
- Never assert implementation-identical behavior (SDP text, candidate order,
  error strings, timing) in tests — WIT-observable outcomes only.
- When a target genuinely cannot support a feature, add a manifest
  `unsupported` entry with a reason instead of weakening the test.
- When a test fails due to a known divergence, add an `expected-fail` entry
  with a tracking reference instead of skipping or deleting the test.
- Keep the two demo hosts' behavior in sync; conformance tests must not change
  production host behavior except where a phase explicitly calls for it.
- Ask the reviewer rather than guessing when a phase's "done when" criteria
  cannot be met as written.
