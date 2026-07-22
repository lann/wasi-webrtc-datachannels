# TODO

Findings from a fresh, rigorous review of the repository against the project
goal: *multiple production-quality implementations of the WebRTC
`peer-connection` / `data-channel` component-model interfaces that run the same
wasm component with compatible (not necessarily identical) behavior across
browsers, mobile, and cloud.*

Items are grouped by area and ordered roughly by impact. File references are
relative to the repository root. Resolved items are deleted (their history
lives in the commits and PRs that fixed them); item ids stay stable, so the
lettering has gaps.

## A. Strategic / whole-project

### A3. Cross-host conformance: loopback matrix + labs in place; interop matrix incomplete

The suite (see `conformance/README.md`) is built and green in CI: a shared
conformance guest, the `conformance-signalingd` mailbox, adapters for
`wasmtime`, `jco-node`, `jco-browser`, and `wasip3-guest`, the
`wasmtime`<->`jco-node` interop pair (both orders) — all run in CI over
loopback via `just conformance` — plus the Shadow lab in CI (non-loopback,
deterministic) and the workstation-only netns lab (`just conformance-netns` /
`just conformance-nat`) covering `lan`, `stun-srflx` (behind a port-restricted
cone NAT), `turn-relay`, and `nat-symmetric`. No manifest expected-fails
remain. Still open:

- **Full interop matrix.** No jco-browser interop pairs exist, and the
  interop pairs run over loopback only.
- **NAT-matrix confirmation.** The NAT scenarios are built and verified
  statically, but a clean `just conformance-nat` run on a workstation (real
  kernel, root) has not been confirmed; they briefly ran as a nightly
  continue-on-error CI job before the netns lab was made workstation-only.
- **netns-lab peer coverage.** The lab's `--peer-kind` covers `wasmtime` (all
  scenarios) and `wasip3-guest` (`lan` only — the in-guest sans-I/O stack
  supports no STUN/TURN); a jco-node lab peer (a per-peer Node runner placed
  in a namespace) is deferred.

## B. Correctness bugs (both hosts unless noted)

### B2. `open-echo` / handshake can hang forever — no timeout

Nothing bounds how long the open handshake may take, so a failed ICE/DTLS
negotiation hangs the guest's `open-echo` indefinitely (only the CI job timeout
would catch it):

- Wasmtime: `examples/wasmtime-demo/src/main.rs` (`open_rx.await`) and the
  manual host's `connect`/gathering waits.
- jco: `waitOpen` (`jco-impl/webrtc.js:219`) and the `openEcho` SDP/ICE
  sequence have no timeout.

Add a bounded wait that surfaces `error::timed-out` (item D1) and also react to
`connectionstatechange`/`iceconnectionstatechange` = `failed` for fast failure.

## C. WIT interface design

### C1. `peer-connection` semantics to pin down in the WIT docs

The interface is now implemented across all three stacks, which settled some of these de
facto; specify them in `wit/webrtc.wit` doc comments so implementations stay
aligned: (a) end-of-candidates on `local-ice-candidates` is signaled by the
stream ending (rather than a browser-style null candidate) — document it; (b)
connection-state observability beyond the one-shot `wait-connected` — there is
no way to observe `disconnected`/`failed` or to re-await; (c) the
`incoming-data-channels` / `local-ice-candidates` streams' once-only semantics;
(d) `close: func()` is sync while the rest is async — confirm that is intended.
Output: revised WIT doc comments.

### C2. `data-channel-options` omits `RTCDataChannelInit` fields without explanation

`data-channel-options` (`wit/webrtc.wit`) exposes only `label`, `ordered`,
`max-retransmits`. Document *why* `protocol`, `max-packet-life-time`,
`negotiated`/`id` were left out, and note that `max-retransmits` and
`max-packet-life-time` are mutually exclusive upstream (so if the latter is ever
added it cannot simply be a sibling `option`).

### C3. Terminology: keep "signaling" out of the design-target prose

The WIT surface is `peer-connection`, but prose has previously referenced a
"`signaling` interface/design target"; the known instances have been corrected.
Keep future docs from reintroducing the name (it now only legitimately names
the demo-only `manual-signaling` interface and the conformance signaling
server).

## D. Error handling

### D1. Typed `error` variants: wasmtime host now classifies; jco does not

`error` declares `closed`, `timed-out`, `invalid-signaling`,
`receiving-via-stream`, `other` (`wit/webrtc.wit:13-28`). The wasmtime host now
produces all of them where they apply (`wasmtime-impl/src/host.rs`:
`InvalidSignaling` on SDP parse/rollback, `TimedOut` on bounded waits,
`Closed`, `ReceivingViaStream`), but many fallible paths still collapse to
`other(string)` (item D2), and the jco host only ever rejects with
`{ tag: 'closed' }` (`jco-impl/webrtc.js`). Wire real classification in the jco
host too (SDP parse → `invalid-signaling`, open/gather timeout → `timed-out`
per item B2, mid-send close → `closed`), with the conformance error-taxonomy
probes asserting it. Consider aligning with WASI 0.3 `error-context` before
stabilizing.

### D2. Host errors are flattened to strings at many call sites

Every fallible host path does `Error::Other(e.to_string())`
(`wasmtime-impl/src/host.rs`, `examples/wasmtime-demo/src/manual.rs`),
discarding the `anyhow`/`webrtc-rs` source chain and giving classification
(item D1) no single home. Follow the `wasmtime-wasi-http` pattern: a
crate-level error type with `From` conversions into the WIT variant, replacing
the ad-hoc `map_err`s.

## E. Implementations

### E1. jco host does not implement `send-via-stream` / `receive-via-stream`

`connections.data-channel` declares four transport methods, but the jco
`DataChannel` (`jco-impl/webrtc.js`, and the fuller
`conformance/adapters/jco/webrtc.js`) implements only `label`, `send`,
`receive`. The streaming methods are simply absent — a parity gap with the
Wasmtime host, surfaced by the conformance suite as `skip-unsupported` on the
streaming tests. Implement them to close the gap.

### E3. `wasip3-impl` limitations to document or lift

- The exported `peer-connection` binds its socket on the IP address named by
  the `WEBRTC_UDP_BIND_ADDR` environment variable, defaulting to IPv4 loopback
  (`wasip3-impl/src/provider.rs`). Loopback connects peers on the **same host**
  (which is what the composed integration test needs); a routable address
  gives the peer a host candidate reachable across a real (non-loopback)
  network path, exercised by the conformance Shadow lab.
- `receive` / `wait-connected` / `incoming-data-channels` poll the shared state
  on a fixed `POLL_NANOS` interval rather than waking on a condition. Adequate,
  but a condition/notify primitive would remove the idle wakeups.
- The in-guest handshake occasionally stalls: both peers reach a state where the
  sans-I/O core reports no pending timer (`poll_timeout` returns `None`) and no
  transmit, each waiting on the other, so `wait-connected` surfaces
  `error::timed-out` (bounded by `CONNECT_TIMEOUT`). This is an upstream `rtc`
  sans-I/O timing issue; `examples/webrtc-consumer` retries a bounded number of
  fresh attempts to keep the integration test reliable. Root-cause and fix
  upstream (or in its driving contract) to make a single attempt deterministic.
- The `rtc` dependency is pinned to an upstream `master` commit (`Cargo.toml`
  `[patch.crates-io]`, `rtc = { git = "https://github.com/webrtc-rs/rtc.git",
  rev = … }`) because the empty-message receive fix
  ([`webrtc-rs/rtc#131`](https://github.com/webrtc-rs/rtc/pull/131), merged
  upstream) is not yet in any published release. Drop the patch and return to a
  published, stable `0.20` once a release including it ships.

## F. Examples

### F1. Demos count bytes but never verify content or ordering

The **conformance suite** now verifies payload content, ordering, and message
boundaries across all targets (`conformance/guest/src/lib.rs`), so divergence
is caught in CI. The remaining gap is demo-local: `examples/echo-demo/src/lib.rs`
tags each message with its index but `run` only counts messages/bytes and never
validates payloads (the Wasmtime demo does not even assert `bytes_echoed`), and
`examples/cli-signaling/src/lib.rs` does not verify the peer message. Low
priority now that conformance covers the property; verify in the demos too or
leave them as pure throughput demos.

### F3. Wire up `rendezvous` end-to-end (tracking)

`demo:webrtc-echo/rendezvous` (`examples/echo-demo/wit/webrtc-echo-demo.wit`) is
defined but imported by no world and implemented by neither host. Per AGENTS.md,
the intended flagship example is two separate component instances (offerer /
answerer) connecting via `peer-connection` (now implemented everywhere) + a
`rendezvous` host that relays SDP/ICE over `wasi:http@0.3` (Wasmtime) / `fetch`
(jco) through a trivial local mailbox server (the conformance
`conformance-signalingd` is a ready-made candidate). This would exercise nearly
every interface at once and could replace the `connect` shortcut as the
reference example.

### F4. Drive the sans-I/O `rtc` stack across a real network (tracking)

`wasip3-impl` is now a **component** that runs the sans-I/O `rtc` stack
in-guest and exports the project `connections` interface, composed (`wac plug`)
with `examples/webrtc-consumer` for the same-host round-trip integration test.
The remaining step is a real deployment across separate machines: the consumer
chooses the bind address through `WEBRTC_UDP_BIND_ADDR` (which produces a
routable host candidate, exercised across a non-loopback simulated network by
the conformance Shadow lab); combined with `rendezvous` (item F3), two separate
components can then connect across a network.
Host-candidate gathering must stay explicit (`ifaces()` is `Unsupported` on
wasm).

## G. Development environment / CI

### G1. jco transpile flags are not checked against the WIT

Any interface/method rename must be mirrored by hand in the
`--async-exports` / `--async-imports` / `--map` strings in
`jco-impl/package.json:9` (AGENTS.md documents this), but nothing verifies it —
a mismatch fails only at transpile or runtime. Add a CI check (or generate the
flags from the WIT) so a drifted rename fails fast with a clear message.

## Suggested priority

1. Correctness the demos can already hit: the open/handshake timeout (B2).
2. Finish the error taxonomy in the jco host (D1) with the crate-level error
   type (D2).
3. Interface-stabilizing decisions (C1, C2).
4. Strategic build-out: port the conformance adapter's `peer-connection`
   implementation to the demo jco host (`jco-impl/webrtc.js`, which still
   implements only the `openEcho` shortcut), wire `rendezvous` (F3), and take
   `wasip3`'s WIT-speaking component to a real network (F4).
5. Cheap hygiene: streaming parity in the jco host (E1), the transpile-flag CI
   check (G1), the remaining conformance-matrix gaps (A3), demo payload
   verification (F1).
