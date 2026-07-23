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

### B2. The manual-signaling handshake can hang forever — no timeout

The demo manual-signaling host's `connect`/gathering waits
(`examples/wasmtime-demo/src/manual.rs`) are unbounded, so a failed ICE/DTLS
negotiation hangs indefinitely (only the CI job timeout would catch it). The
`open-echo` half of this item is gone: the echo demo now drives the standard
`connections` interface, whose `wait-connected` is bounded on every
implementation (asserted by the `error-timed-out` conformance probe). The
remaining fix is to rebuild `cli-signaling` on the standard interface too,
deleting the bespoke manual host rather than bounding it.

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

### C4. Consider aligning `error` with WASI 0.3 `error-context`

Before stabilizing the interface, evaluate whether the `types.error` variant
should align with (or be replaced by) WASI 0.3's `error-context` mechanism,
which is the component-model-native way to attach contextual failure
information to async operations.

## D. Error handling

### D2. Host errors are flattened to strings at many call sites

Every fallible host path does `Error::Other(e.to_string())`
(`wasmtime-impl/src/host.rs`, `examples/wasmtime-demo/src/manual.rs`),
discarding the `anyhow`/`webrtc-rs` source chain and giving error
classification no single home. Follow the `wasmtime-wasi-http` pattern: a
crate-level error type with `From` conversions into the WIT variant, replacing
the ad-hoc `map_err`s.

## E. Implementations

### E3. Unwind the `rtc` git pin once upstream ships a release

The `rtc` dependency is pinned to an upstream `master` commit (`Cargo.toml`
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
every interface at once and would make the echo demo's two peers genuinely
separate components, making it the reference example.

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
2. Give host errors a crate-level type instead of flattened strings (D2).
3. Interface-stabilizing decisions (C1, C2).
4. Strategic build-out: wire `rendezvous` (F3) and take `wasip3`'s
   WIT-speaking component to a real network (F4).
5. Cheap hygiene: the transpile-flag CI check (G1), the remaining
   conformance-matrix gaps (A3), demo payload verification (F1).
