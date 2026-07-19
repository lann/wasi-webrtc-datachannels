# TODO

Findings from a fresh, rigorous review of the repository against the project
goal: *multiple production-quality implementations of the WebRTC
`peer-connection` / `data-channel` component-model interfaces that run the same
wasm component with compatible (not necessarily identical) behavior across
browsers, mobile, and cloud.*

Items are grouped by area and ordered roughly by impact. File references are
relative to the repository root and were verified against the tree at review
time. A handful of small, unambiguous fixes were made in the same change that
introduced this file (see **Fixed opportunistically** at the end); everything
below is still open.

Note on the previous TODO.md: it referenced a `signaling` interface at
`wit/webrtc.wit` that no longer exists (the guest-driven surface is now the
`connections.peer-connection` resource), and an `echo-demo` receive path that
has since been rewritten. It was stale and is fully replaced here.

## A. Strategic / whole-project

### A2. The guest-driven `peer-connection` path is unimplemented in the two host stacks

`connections.peer-connection` (`wit/webrtc.wit:197-225`) is the design target for
guest-driven connection setup. The `wasip3-impl` **component** now implements the
whole `connections` surface — including `peer-connection` (offer/answer +
trickle-ICE) — driven in-guest over `wasi:sockets`, and is exercised by the
composed `examples/webrtc-consumer` round-trip integration test
(`just test-webrtc-composed`). But **both hosts** still trap on every
`peer-connection` method (`wasmtime-impl/src/host.rs:411-495` returns
`peer_connection_unsupported()`; `jco-impl/webrtc.js` implements only `openEcho`
+ `DataChannel`). The only working host path is the `connect` convenience
shortcut, where the host builds *both* peers internally. Implement
`peer-connection` in at least one of the two hosts so the interface is proven
host-side too, and settle its open design questions (item C1).

### A3. No cross-host conformance is executed; the suite is a plan only

`conformance/PLAN.md` (21 KB) describes a behavioral + interop conformance suite,
but `conformance/` contains **only** that plan — no runner, guest, adapters, or
`just conformance` recipe, and no workspace members for it. Meanwhile real
divergences between the two working hosts already exist and are unguarded:
typed-error taxonomy (item D1), `send-via-stream`/`receive-via-stream` presence
(item B4), and channel-close/peer-close semantics (items B2/B3). Stand up at
least Phase 0 of the plan — one shared conformance guest asserting WIT-level
outcomes (variant tags, payload bytes, ordering under `ordered:true`, resource
lifecycle) against both hosts in CI — so divergences fail fast.

## B. Correctness bugs (both hosts unless noted)

### B1. Inbound message path is unbounded — no guest→SCTP backpressure

Every received message is pushed into an unbounded queue with no reader-driven
backpressure, so a slow guest reader grows host memory without limit:

- Wasmtime echo host: `examples/wasmtime-demo/src/main.rs:148`
  (`mpsc::unbounded::<InboundMessage>()`), fed from `on_message`.
- Wasmtime manual host: the same shape in
  `examples/wasmtime-demo/src/manual.rs` / `wasmtime-impl/tests/manual_host.rs`.
- jco host: `jco-impl/webrtc.js:136-156` (`incomingQueue`'s `messages[]` array
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

- Wasmtime: `examples/wasmtime-demo/src/main.rs:177` (`open_rx.await`) and the
  manual host's `connect`/gathering waits.
- jco: `waitOpen` (`jco-impl/webrtc.js:178-184`) and the `openEcho` SDP/ICE
  sequence have no timeout.

Add a bounded wait that surfaces `error::timed-out` (item D1) and also react to
`connectionstatechange`/`iceconnectionstatechange` = `failed` for fast failure.

### B3. jco host never closes its peer connections (native resource leak)

`openEcho` creates `near`/`far` `RTCPeerConnection`s and retains them in
`DataChannel.#keepAlive` (`jco-impl/webrtc.js:94-95,126`), but nothing ever
calls `pc.close()` and the resource exposes no dispose hook, so under
`@roamhq/wrtc` every channel leaks native ICE/DTLS/SCTP threads and sockets for
the process lifetime. The Wasmtime host does this correctly via
`Drop for DataChannel` → `close_peer_connections`
(`wasmtime-impl/src/data_channel.rs`). Give the jco `DataChannel` a
close/`Symbol.dispose` path that closes both peers.

### B4. jco `send` and the far-side echo can trap instead of returning an error

`DataChannel.send` (`jco-impl/webrtc.js:58-63`) calls `this.#channel.send(...)`
without a try/catch and without checking `readyState`; for a
`result<_, error>`-returning import, a thrown JS error traps the component
instead of surfacing `result::err`. The far-side echo handler
(`jco-impl/webrtc.js:104`, `channel.onmessage = ({data}) => channel.send(data)`)
is likewise unguarded — if that send throws, the message is dropped and the
near side's `receive` can deadlock. Catch and convert to the WIT `error` variant
(item D1); guard the echo send.

### B5. Wasmtime peer close depends on a live Tokio runtime

`close_peer_connections` (`wasmtime-impl/src/data_channel.rs:158-171`) spawns
`pc.close()` on the current runtime and, if `Handle::try_current()` fails,
silently drops the `Arc`s — leaking `webrtc-rs` background tasks. This is an
edge case (Drop after the runtime stops), but a production host should not rely
on runtime presence for cleanup. Consider a dedicated close path or documenting
the invariant.

### B6. Wasmtime host cannot receive zero-length messages (upstream webrtc-rs bug)

`webrtc-rs`'s `RTCDataChannel::read_loop` treats any zero-byte read from
`read_data_channel` as EOF and closes the channel
(`webrtc-0.17/src/data_channel/mod.rs`, the `Ok((0, _))` arm) — but per RFC
8831 §6.6 a zero-length message legitimately arrives as a zero-byte read with a
`StringEmpty`/`BinaryEmpty` PPID, which `webrtc-data` correctly decodes to
`n = 0`. So a peer that receives an empty message through the callback API has
its channel torn down instead of observing the message. Sending empty messages
works (`webrtc-data` maps them onto the empty PPIDs); only receiving is broken.
Tracked by the `zero-length-message` expected-fail in
`conformance/manifests/wasmtime.toml`; fixing it needs an upstream patch or
detached data channels with a host-side read loop.

## C. WIT interface design

### C1. `peer-connection` has open design questions to settle before implementation

Before item A2 implements it, resolve on `wit/webrtc.wit:197-225`: (a) how
end-of-candidates is signaled on `local-ice-candidates` (browsers use a null
candidate; a `stream` end may suffice but should be specified); (b)
connection-state observability beyond the one-shot `wait-connected` — there is no
way to observe `disconnected`/`failed` or to re-await; (c) the
`incoming-data-channels` / `local-ice-candidates` streams' once-only semantics;
(d) `close: func()` is sync while the rest is async — confirm that is intended.
Output: revised WIT + doc comments.

### C2. `data-channel-options` omits `RTCDataChannelInit` fields without explanation

`data-channel-options` (`wit/webrtc.wit:31-40`) exposes only `label`, `ordered`,
`max-retransmits`. Document *why* `protocol`, `max-packet-life-time`,
`negotiated`/`id` were left out, and note that `max-retransmits` and
`max-packet-life-time` are mutually exclusive upstream (so if the latter is ever
added it cannot simply be a sibling `option`). (The misleading "Defaults to
`true`" comment on `ordered` was fixed opportunistically — see the end.)

### C3. Terminology: docs still call the design target "signaling" in places

The WIT surface is `peer-connection`, but scattered prose still references a
"`signaling` interface/design target." The Rust host copies were corrected
opportunistically (see the end); re-check README/AGENTS and future docs so the
`signaling` name does not creep back in (it now only legitimately names the
demo-only `manual-signaling` interface).

## D. Error handling

### D1. Typed `error` variants are essentially never produced

`error` declares `closed`, `timed-out`, `invalid-signaling`,
`receiving-via-stream`, `other` (`wit/webrtc.wit:13-28`), but across the whole
repo only `closed` and `receiving-via-stream` are ever produced
(`wasmtime-impl/src/host.rs:74,305,322,324,377`); `timed-out` and
`invalid-signaling` appear nowhere, and everything else collapses to
`other(string)`. The jco host only ever rejects with `{ tag: 'closed' }`
(`jco-impl/webrtc.js:162`). Either wire real classification (SDP parse →
`invalid-signaling`, open/gather timeout → `timed-out` per item B2, mid-send
close → `closed`) in both hosts with tests, or trim the unproduced variants.
Consider aligning with WASI 0.3 `error-context` before stabilizing.

### D2. Host errors are flattened to strings at many call sites

Every fallible host path does `Error::Other(e.to_string())`
(`wasmtime-impl/src/host.rs:56,65,82,350`,
`examples/wasmtime-demo/src/manual.rs` and its test twin), discarding the
`anyhow`/`webrtc-rs` source chain and giving classification (item D1) no single
home. Follow the `wasmtime-wasi-http` pattern: a crate-level error type with
`From` conversions into the WIT variant, replacing the ad-hoc `map_err`s.

## E. Implementations

### E1. jco host does not implement `send-via-stream` / `receive-via-stream`

`connections.data-channel` declares four transport methods, but the jco
`DataChannel` (`jco-impl/webrtc.js:36-86`) implements only `label`, `send`,
`receive`. The streaming methods are simply absent — a parity gap with the
Wasmtime host that is invisible today only because `echo-demo` never calls them
(and the transpile flags don't map them, item F1). Implement them (or document
the gap and make item A3's conformance suite assert it).

### E2. ~380-line verbatim duplication of the manual-signaling host

`wasmtime-impl/tests/manual_host.rs` (385 lines) and
`examples/wasmtime-demo/src/manual.rs` (397 lines) are identical except for the
doc header and the bindgen `path` (verified by diff — one differing line of
code). Any fix to `ManualPeer` must be made twice and can silently drift. Move
`ManualPeer` into one shared location (e.g. a module in `wasmtime-impl` reused as
a dev-dependency by the test and by the demo binary).

### E3. `wasip3-impl` limitations to document or lift

- The exported `peer-connection` binds its socket on IPv4 loopback
  (`wasip3-impl/src/provider.rs`), so it only connects peers on the **same host**
  (which is what the composed integration test needs). Real (non-loopback)
  networking needs a way for the consumer to choose the bind address and a
  routable host candidate.
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
- Zero-length messages arrive corrupted: the `rtc` receive path
  (`rtc/src/peer_connection/handler/datachannel.rs`) keeps the one-zero-byte
  SCTP placeholder payload for the `BinaryEmpty`/`StringEmpty` PPIDs instead of
  mapping it back to an empty message, so a received empty message surfaces as a
  single zero byte. Upstream `rtc` bug; `zero-length-message` is expected-fail
  in `conformance/manifests/wasip3-guest.toml` (and in the wasmtime interop-pair
  manifests, where the wasmtime peer has the analogous B6 receive bug).
- The `rtc` dependency is pinned to a `0.20` release candidate
  (`Cargo.toml`, `rtc = "0.20.0-rc.3"`). Published on crates.io, but a
  pre-release on the critical path — track moving to a stable `0.20` once it
  ships.

## F. Examples

### F1. Demos count bytes but never verify content or ordering

`make_message` tags each message with its index precisely so ordering/integrity
could be checked (`examples/echo-demo/src/lib.rs:87-93`), but `run` only counts
messages/bytes (`:50-64`) and never validates payloads; the Wasmtime demo does
not even assert `bytes_echoed`. `examples/cli-signaling/src/lib.rs` likewise
does not verify the peer message. Verify content + order in the echo guest and
the manual-signaling test so a host that corrupts, reorders (under
`ordered:true`), or duplicates messages fails the demos. This is the cheap
precursor to the conformance suite (item A3).

### F2. Dead demo WIT surface referencing a nonexistent `browser-signaling` component

`examples/cli-signaling/wit/webrtc-echo-demo.wit` defines the `prompt` and
`manual-demo` interfaces and the `browser-signaling-demo` world (`:11-23,26-48,
104-110`), and the crate/module docs (`examples/cli-signaling/src/lib.rs:10-11`)
refer to a sibling `browser-signaling` component "which speaks the exact same
wire format" — but no such component, world instantiation, or jco host support
exists (the only implemented world is `manual-signaling-host`, driven by
`cli-signaling`, which exports `wasi:cli/run`). Either build the browser
manual-signaling demo (it would be the first exercise of the jco host beyond
echo) or delete the unused `prompt`/`manual-demo`/`browser-signaling-demo`
surface and the dangling references.

### F3. Wire up `rendezvous` end-to-end (tracking)

`demo:webrtc-echo/rendezvous` (`examples/echo-demo/wit/webrtc-echo-demo.wit`) is
defined but imported by no world and implemented by neither host. Per AGENTS.md,
the intended flagship example is two separate component instances (offerer /
answerer) connecting via `peer-connection` (item A2) + a `rendezvous` host that
relays SDP/ICE over `wasi:http@0.3` (Wasmtime) / `fetch` (jco) through a trivial
local mailbox server. This would exercise nearly every interface at once and
could replace the `connect` shortcut as the reference example.

### F4. Drive the sans-I/O `rtc` stack across a real network (tracking)

`wasip3-impl` is now a **component** that runs the sans-I/O `rtc` stack
in-guest and exports the project `connections` interface, composed (`wac plug`)
with `examples/webrtc-consumer` for the same-host round-trip integration test.
The remaining step is real (non-loopback) networking: let the consumer choose
the bind address and produce a routable host candidate, then, combined with
`rendezvous` (item F3), let two separate components connect across a network.
Host-candidate gathering must stay explicit (`ifaces()` is `Unsupported` on
wasm).

## G. Development environment / CI

### G1. jco transpile flags are not checked against the WIT

Any interface/method rename must be mirrored by hand in the
`--async-exports` / `--async-imports` / `--map` strings in
`jco-impl/package.json:9` (AGENTS.md documents this), but nothing verifies it —
a mismatch fails only at transpile or runtime. Add a CI check (or generate the
flags from the WIT) so a drifted rename fails fast with a clear message.

### G3. The jco Node host path is never exercised in CI

CI runs only `just test-browser` (headless Chrome). The Node demo
(`jco-impl/src/run.mjs`, `npm test`) shares `webrtc.js` but exercises the
`@roamhq/wrtc` backing and JSPI under Node, which the browser test does not.
Add a Node host run to CI (cheap — no Rust needed beyond the shared component
build).

## Suggested priority

1. Correctness the demos can already hit: inbound backpressure (B1), open
   timeout (B2), jco peer close + error trapping (B3, B4).
2. Make divergence visible: payload/ordering verification (F1) → a Phase-0
   conformance guest across both hosts (A3), which also pins the error taxonomy
   (D1) and streaming parity (E1).
3. Interface-stabilizing decisions (C1, C2) and the crate-level error type (D2).
4. Strategic build-out: implement `peer-connection` host-side (A2), wire
   `rendezvous` (F3), and take `wasip3`'s WIT-speaking component to a real
   network (F4).
5. Cheap hygiene: de-duplicate the manual host (E2), delete or build the dead
   `browser-signaling` surface (F2), add the CI gaps (G1, G3), document the
   `wasip3` limits (E3).

## Fixed opportunistically in this change

- Removed dead public API `send_message` (`wasmtime-impl/src/data_channel.rs`)
  and `WasiWebrtcCtx::configure_setting_engine` (`wasmtime-impl/src/lib.rs`),
  both exported but unused anywhere in the repo.
- Dropped the unused `@bytecodealliance/componentize-js` devDependency from
  `jco-impl/package.json` (the component is built from Rust via `wasm-tools`;
  nothing invokes it).
- Fixed the misleading "Defaults to `true`" comment on `data-channel-options.
  ordered` in `wit/webrtc.wit` (WIT records have no defaults).
- Corrected the `jco-impl/webrtc.js` header comment that claimed the host uses
  the WHATWG `ReadableStream` API (it uses a plain promise queue).
- Replaced stale "`signaling` design target" wording with "peer-connection" /
  "guest-driven connection design target" in the Wasmtime host docs
  (`wasmtime-impl/src/{lib,host,bindings}.rs`) and the `cli-signaling` WIT.
