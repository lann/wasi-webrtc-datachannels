# TODO

Findings from a fresh, rigorous review of the repository against the project
goal: *multiple production-quality implementations of the WebRTC
`peer-connection` / `data-channel` component-model interfaces that run the same
wasm component with compatible (not necessarily identical) behavior across
browsers, mobile, and cloud.*

Items are grouped by area and ordered roughly by impact. File references are
relative to the repository root. Items resolved since the file was introduced
are marked **Fixed** in place (with a short note of how) so the history stays
findable without rehashing it.

## A. Strategic / whole-project

### A2. The guest-driven `peer-connection` path — Fixed (demo jco host still echo-only)

**Fixed** in substance: `connections.peer-connection` is now implemented by
the Wasmtime host (`wasmtime-impl/src/host.rs` `HostPeerConnection*` +
`wasmtime-impl/src/peer_connection.rs`), by the conformance jco host
(`conformance/adapters/jco/webrtc.js`, run under Node and headless Chrome),
and by the in-guest `wasip3-impl` provider — and the conformance suite drives
real offer/answer + trickle-ICE signaling across all of them. The one remaining
gap is the *demo* jco host (`jco-impl/webrtc.js`), which still implements only
the `openEcho` shortcut + `data-channel`; port the conformance adapter's
`peer-connection` implementation there if the demo should exercise it.

### A3. Cross-host conformance: loopback matrix + ICE lab in place; NAT still open

`conformance/PLAN.md` is now implemented through Phase 5: a shared conformance
guest, the `conformance-signalingd` mailbox, adapters for `wasmtime`,
`jco-node`, `jco-browser`, and `wasip3-guest` (the guest composed with the
in-guest `wasip3-impl` provider, run under `wasmtime run`), the
`wasmtime`<->`jco-node` interop pair (both orders) — all run in CI over
loopback via `just conformance` — and the ICE lab (`just conformance-ice`,
CI job 2): the wasmtime two-peer corpus over a routed network-namespace
topology exercising real non-loopback paths (`lan` direct and `turn-relay`
through coturn). No manifest expected-fails remain. The
`wasmtime`<->`wasip3-guest` pairs are wired into `conformance-interop` but
disabled by default pending the teardown-flush fix (item E3). Still open from
the plan: NAT on the router so `stun-srflx` is meaningful (Phase 6).

## B. Correctness bugs (both hosts unless noted)

### B1. Inbound message path is unbounded — no guest→SCTP backpressure

Every received message is pushed into an unbounded queue with no reader-driven
backpressure, so a slow guest reader grows host memory without limit:

- Wasmtime host: `wasmtime-impl/src/data_channel.rs:121`
  (`mpsc::unbounded::<InboundMessage>()`), fed from the channel pump; the demo
  and manual hosts inherit it.
- jco host: `jco-impl/webrtc.js:177` (`incomingQueue`'s `messages[]` array
  grows unbounded on `"message"`).

Outbound backpressure exists (Wasmtime relies on the async ABI; jco gates on
`bufferedAmount`, `webrtc.js:74-85`), but the inbound side does not. Given the
memory-usage priority, switch to a bounded queue with a documented policy (pause
via a `ready` gate, or an explicit "receive buffer full ⇒ close with error"),
mirrored in both hosts.

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

### B3. jco host never closes its peer connections (native resource leak)

`openEcho` creates `near`/`far` `RTCPeerConnection`s and retains them in
`DataChannel.#keepAlive` (`jco-impl/webrtc.js`), but nothing ever
calls `pc.close()` and the resource exposes no dispose hook, so under
`@roamhq/wrtc` every channel leaks native ICE/DTLS/SCTP threads and sockets for
the process lifetime. The Wasmtime host does this correctly via
`Drop for DataChannel` → `close_peer_connections`
(`wasmtime-impl/src/data_channel.rs`), and the conformance jco adapter closes
its peers (`conformance/adapters/jco/webrtc.js`). Give the jco `DataChannel` a
close/`Symbol.dispose` path that closes both peers.

### B4. jco `send` and the far-side echo can trap instead of returning an error

`DataChannel.send` (`jco-impl/webrtc.js:63`) calls `this.#channel.send(...)`
without a try/catch and without checking `readyState`; for a
`result<_, error>`-returning import, a thrown JS error traps the component
instead of surfacing `result::err`. The far-side echo handler
(`jco-impl/webrtc.js:144`, `channel.onmessage = ({data}) => channel.send(data)`)
is likewise unguarded — if that send throws, the message is dropped and the
near side's `receive` can deadlock. Catch and convert to the WIT `error` variant
(item D1) as the conformance jco adapter does; guard the echo send.

### B5. Wasmtime peer close depends on a live Tokio runtime

`close_peer_connections` (`wasmtime-impl/src/data_channel.rs:319-330`) spawns
`pc.close()` on the current runtime and, if `Handle::try_current()` fails,
silently drops the `Arc`s — leaking `webrtc-rs` background tasks. This is an
edge case (Drop after the runtime stops), but a production host should not rely
on runtime presence for cleanup. Consider a dedicated close path or documenting
the invariant.

### B6. Wasmtime host cannot receive zero-length messages (upstream webrtc-rs bug) — Fixed

**Fixed** by moving the wasmtime host off the async `webrtc` 0.17 crate onto
`webrtc` 0.20 (rebuilt on the sans-I/O `rtc` crate), whose per-channel poll loop
(`wasmtime-impl/src/data_channel.rs`) delivers every `OnMessage` event —
including an empty payload — instead of conflating a zero-byte read with
end-of-stream as 0.17's `read_loop` did. The workspace patches `rtc` to the
`lann/rtc` fork carrying the empty-message receive fix (upstream PR
[`webrtc-rs/rtc#131`](https://github.com/webrtc-rs/rtc/pull/131)); the
`zero-length-message` expected-fails were removed from the manifests.

## C. WIT interface design

### C1. `peer-connection` semantics to pin down in the WIT docs

The interface is now implemented (item A2), which settled some of these de
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

### E2. ~380-line verbatim duplication of the manual-signaling host — Fixed

**Fixed**: the duplicate `wasmtime-impl/tests/manual_host.rs` was deleted; the
integration test now reuses the single implementation at
`examples/wasmtime-demo/src/manual.rs` through a dev-dependency on the
`wasmtime-webrtc-host` crate.

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
  fresh attempts to keep the integration test reliable. Root-cause and fix in the
  fork (or its driving contract) to make a single attempt deterministic.
- ~~Zero-length messages arrive corrupted: a received empty message surfaces as
  a single `0x00` byte.~~ **Fixed** on the `lann/rtc` fork the workspace now
  tracks: `rtc`'s `DataChannelHandler::handle_read` maps the `BinaryEmpty` /
  `StringEmpty` PPIDs back to an empty payload before surfacing the message
  (per RFC 8831 §6.6; the send path already did the inverse mapping).
  `zero-length-message` passes on `wasip3-guest` and in the wasmtime
  interop-pair manifests (the wasmtime peer's analogous B6 bug is also fixed).
  Submitted upstream as
  [`webrtc-rs/rtc#131`](https://github.com/webrtc-rs/rtc/pull/131); drop the
  fork and return to a published `rtc` once that merges and ships (tracked with
  the release-candidate bullet below).
- The `wasmtime`<->`wasip3-guest` interop pair stalls deterministically (every
  test, every attempt): packet capture shows the full ICE/DTLS/SCTP handshake
  and both 16-message payload bursts complete within ~120 ms, but the wasip3
  peer's barrier sentinel never reaches the wire and no SCTP/DTLS close is
  sent — the guest's `peer.close()` returns after queueing, the driver exits,
  and the process death cuts the detached runtime pump before the sentinel or
  the close handshake flushes. Meanwhile the wasip3 peer itself reports
  **pass** (its `receive` surfaced `closed` early), so the failure is
  one-sided: the wasmtime (webrtc-rs) peer retransmits its own unacked
  sentinel forever (no receive timeout, item B2) and the whole attempt hits the
  orchestrator's timeout. Fix direction: make the wasip3 provider's
  close/drop path flush the pending SCTP send queue and complete (or at least
  emit) the SCTP/DTLS close before the driver exits — e.g. drain
  `poll_transmit` to quiescence after `close()` and only then return from
  `wasi:cli/run`. Until then the pair cannot be enabled in CI.
- The `rtc` dependency is pinned to the `lann/rtc` fork's `master`
  (`Cargo.toml`, `rtc = { git = "https://github.com/lann/rtc.git", rev = … }`),
  which carries the empty-message receive fix (upstream PR
  [`webrtc-rs/rtc#131`](https://github.com/webrtc-rs/rtc/pull/131)) on top of the
  published `0.20.0-rc.3` release candidate. Two things on the critical path to
  unwind: a git fork instead of a crates.io release, and a pre-release base.
  Return to a published, stable `0.20` once PR #131 merges upstream and a release
  including it ships.

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

### F2. Dead demo WIT surface referencing a nonexistent `browser-signaling` component — Fixed

**Fixed**: the unused `prompt` / `manual-demo` interfaces and the
`browser-signaling-demo` world were deleted from
`examples/cli-signaling/wit/webrtc-echo-demo.wit`, along with the dangling
`browser-signaling` references in the crate docs. A browser manual-signaling
demo can reintroduce them if it is ever built.

### F3. Wire up `rendezvous` end-to-end (tracking)

`demo:webrtc-echo/rendezvous` (`examples/echo-demo/wit/webrtc-echo-demo.wit`) is
defined but imported by no world and implemented by neither host. Per AGENTS.md,
the intended flagship example is two separate component instances (offerer /
answerer) connecting via `peer-connection` (now implemented, item A2) + a
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

### G3. The jco Node host path is never exercised in CI — Fixed

**Fixed** by the conformance suite: the `jco-node` target (and the
`wasmtime`<->`jco-node` interop pair) runs the guest under Node 24 with
`--experimental-wasm-jspi` and `@roamhq/wrtc` in every CI run
(`.github/workflows/conformance.yml`, `just conformance-jco-node`).

## Suggested priority

1. Correctness the demos can already hit: inbound backpressure (B1), open
   timeout (B2), jco peer close + error trapping (B3, B4).
2. Fix the wasip3 teardown flush (E3) so the `wasmtime`<->`wasip3-guest`
   interop pairs can be enabled in CI, and finish the error taxonomy in the
   jco host (D1) with the crate-level error type (D2).
3. Interface-stabilizing decisions (C1, C2).
4. Strategic build-out: port `peer-connection` to the demo jco host (A2),
   wire `rendezvous` (F3), and take `wasip3`'s WIT-speaking component to a real
   network (F4).
5. Cheap hygiene: streaming parity in the jco host (E1), the transpile-flag CI
   check (G1), NAT for `stun-srflx` (A3), demo payload verification (F1).
