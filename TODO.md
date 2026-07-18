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

### A1. Only two of the three stacks actually implement the shared WIT

The headline promise is "one component, many implementations." Today only
`wasmtime-impl` (Rust host) and `jco-impl` (JS host) implement
`lann:webrtc-datachannels`; the `echo-demo` component runs unchanged against
both — that part works. But `wasip3-impl` does **not** implement the package at
all: it is a standalone sans-I/O `rtc` experiment with its own Rust API, and
`examples/wasip3-cli` exports `wasi:cli/run`, not any project interface (no file
under `wasip3-impl/` references `lann:webrtc-datachannels`,
`connections`, or `peer-connection`). So "third implementation" is currently
aspirational. To make it real, `wasip3-impl` (or a new crate) needs to expose
the `connections.data-channel` / `peer-connection` resources to a guest — the
natural convergence point with items A2 and E.

### A2. The guest-driven `peer-connection` path is unimplemented and untested everywhere

`connections.peer-connection` (`wit/webrtc.wit:197-225`) is the design target for
guest-driven connection setup, but **both** hosts trap on every one of its
methods (`wasmtime-impl/src/host.rs:411-495` returns
`peer_connection_unsupported()`; `jco-impl/webrtc.js` implements only `openEcho`
+ `DataChannel`). The only working path in the whole repo is the `connect`
convenience shortcut, where the host builds *both* peers internally. Until a
host implements `peer-connection` with an offer/answer + trickle-ICE integration
test between two in-process peers, the interface is unproven and its open design
questions (item C1) cannot be settled.

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

- `NativePeer` / `GuestPeer` track a single "primary" data channel and ignore
  any second channel (`wasip3-impl/src/native.rs`, `.../guest.rs`). Document
  this as a known limit or support multiple channels via a channel-id map.
- `NativePeer` drives only the answerer role (the offerer core
  `SansIoPeer::offerer` exists but has no native driver); `GuestPeer` drives
  both, which is why `examples/wasip3-cli` can do a loopback offerer+answerer.
  Note the asymmetry.
- The `rtc` dependency is pinned to a git fork commit
  (`Cargo.toml`, `github.com/lann/rtc` `wasi` branch). Reproducible, but a
  private fork on the critical path — track upstreaming the wasm `ifaces()` stub
  and `socket2 0.6` bump, or vendor it.

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

### F4. Drive the sans-I/O `rtc` stack from a guest that speaks the project WIT (tracking)

`wasip3-impl` already proves the wasm-capable `lann/rtc` fork interoperates with
`webrtc-rs` over real DTLS+SCTP, and `examples/wasip3-cli` runs the whole stack
in-guest over `wasi:sockets` loopback. The remaining step (and the payoff for
item A1) is a guest driver over real (non-loopback) `wasi:sockets` that exposes
the `connections` resources and, combined with `rendezvous` (item F3), lets two
separate components connect across a network. Host-candidate gathering must stay
explicit (`ifaces()` is `Unsupported` on wasm).

## G. Development environment / CI

### G1. jco transpile flags are not checked against the WIT

Any interface/method rename must be mirrored by hand in the
`--async-exports` / `--async-imports` / `--map` strings in
`jco-impl/package.json:9` (AGENTS.md documents this), but nothing verifies it —
a mismatch fails only at transpile or runtime. Add a CI check (or generate the
flags from the WIT) so a drifted rename fails fast with a clear message.

### G2. `wasip3-cli` is only lint-checked in CI, never built or run

`just clippy` covers `wasip3-cli` (`justfile:21`) but neither `just ci` nor
`.github/workflows/ci.yml` builds (`just build-wasip3-cli`) or runs
(`just demo-wasip3-cli`) it, so a break in the in-guest demo (or the `GuestPeer`
driver) ships silently. Add at least a `build-wasip3-cli` step to CI; run
`demo-wasip3-cli` if a `wasmtime` v46+ is available on the runner.

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
4. Strategic build-out: implement `peer-connection` (A2), wire `rendezvous`
   (F3), and give `wasip3` a WIT-speaking guest (A1/F4).
5. Cheap hygiene: de-duplicate the manual host (E2), delete or build the dead
   `browser-signaling` surface (F2), add the CI gaps (G1–G3), document the
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
