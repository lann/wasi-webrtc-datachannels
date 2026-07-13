# TODO

Atomic issues from a full repository review, grouped by priority area. Each item
is scoped to be independently actionable. File references are relative to the
repository root.

## A. WIT interface design

### 5. `error` variants `closed` / `timed-out` / `invalid-signaling` are never produced by any host

Both hosts collapse every failure into `other(string)`
(`wasmtime-impl/src/host.rs:67,120`,
`examples/wasmtime-demo/src/manual.rs:328+`), and the jco host never returns a
typed error at all. Either wire real classification (SDP parse →
`invalid-signaling`, channel closed mid-send → `closed`, gathering/open timeout
→ `timed-out`) or trim the variant. Also consider aligning with the WASI 0.3
`error-context` pattern before stabilizing. Acceptance: at least `closed` and
`invalid-signaling` produced where applicable in both hosts, with tests.

### 6. `data-channel-options` claims defaults it cannot express, and its subset choice is undocumented

`ordered: bool` is required in WIT so the "Defaults to `true`" doc comment
(`wit/webrtc.wit:22-24`) is misleading — WIT records have no defaults. Either
make it `option<bool>` or fix the docs. Separately, document why `protocol`,
`max-packet-life-time`, `negotiated`/`id` were omitted from the
`RTCDataChannelInit` subset (and note
`max-retransmits`/`max-packet-life-time` are mutually exclusive upstream).

### 7. `signaling` interface design gaps before first implementation

The design-target `signaling` interface (`wit/webrtc.wit:67-122`) has open
questions worth settling before any host implements it: (a) how
end-of-candidates is signaled on `local-ice-candidates` (browser uses a null
candidate); (b) no connection-state observability beyond a one-shot
`wait-connected` (no `disconnected`/`failed` transitions, no re-await
semantics); (c) `incoming-data-channels`/`local-ice-candidates` presumably
share the callable-once problem of item 1; (d) `close: func()` sync vs the
async rest. Output: revised WIT + doc comments.

### 8. Implement `signaling` in the Wasmtime host (tracking)

`signaling` is "designed but not exercised". Implement it in
`wasmtime-webrtc-datachannels` behind `add_to_linker` (after item 7), following
the `wasmtime_wasi_http::p3` patterns already used, with an integration test in
`wasmtime-impl/tests` driving offer/answer + trickle ICE between two in-process
peer connections.

## B. Wasmtime host implementation

### 11. Inbound message path is unbounded — no backpressure from guest to SCTP

Both the echo path (`examples/wasmtime-demo/src/main.rs:148`) and manual path
(`manual.rs::wire_channel`) shovel every `on_message` payload into a
`futures::mpsc::unbounded` channel; a slow guest reader means unbounded host
memory. The jco host has the same shape (`incomingStream` enqueues into a
`ReadableStream` without honoring `desiredSize`, `jco-impl/webrtc.js:122-139`).
Given the memory-usage priority, switch to a bounded channel + documented
drop/pause policy (SCTP can't be paused per-message, but a bound with
`ready`-gated enqueue or an explicit "receive buffer full ⇒ close with error"
policy is still better than unbounded), mirrored in both hosts.

### 12. Error fidelity: host errors stringified via `Error::Other(e.to_string())`

All host fallible paths flatten `anyhow` chains into strings (loses source
chain, no downcast). Follow the wasmtime-wasi-http pattern of a crate-level
error type with `From` conversions to the WIT variant so classification
(item 5) has one home, instead of ad-hoc `map_err` at 6+ call sites
(`host.rs`, `manual.rs`, `manual_host.rs`, `main.rs`).

### 13. `PipeProducer` yields one item per `poll_produce` and uses ad-hoc unsafe pinning

`wasmtime-impl/src/pipe.rs` (a) buffers exactly one item per poll
(`Buffer = Option<T>`), forfeiting batching when many messages are already
queued — measure and, if it matters, drain multiple ready items into the
`Destination` per poll; (b) uses `unsafe { map_unchecked_mut }` where
`futures::stream::Stream + Unpin` (the only current user is an
`UnboundedReceiver`, which is `Unpin`) or `pin-project-lite` would remove the
`unsafe`.

### 14. Dead public API: `send_message` and `configure_setting_engine`

`wasmtime-impl` exports `send_message` (`data_channel.rs:96`) and
`WasiWebrtcCtx::configure_setting_engine` (`lib.rs:95`) but no code in the repo
uses either (consumers use `setting_engine_hook()` + `new_peer_connection`).
Remove them or use them (e.g. `new_peer_connection` could take
`&WasiWebrtcCtx` and call `configure_setting_engine`, simplifying the
hook-threading at every call site).

### 15. ~370-line verbatim duplication: `wasmtime-impl/tests/manual_host.rs` vs `examples/wasmtime-demo/src/manual.rs`

The two files differ only in doc headers and the bindgen `path` (verified by
diff). The test host should reuse the demo crate's `manual` module (add
`wasmtime-webrtc-host` as a dev-dependency of `wasmtime-impl`, or move
`ManualPeer` into a shared location) so bug fixes can't drift apart — item 9's
fix would currently need to be made twice.

## C. jco host

### 16. jco host never returns typed WIT errors — failures become traps

`webrtc.js` `send`/`openEcho` let exceptions propagate; for
`result<_, error>`-returning imports jco expects the declared error shape, so a
thrown `Error` traps the component instead of surfacing `result::err`. Catch
and convert to the WIT `error` variant (tagged union form jco expects), aligned
with the classification in item 5.

### 17. `openEcho` (both hosts) can hang forever if ICE/DTLS never completes

`waitOpen` (`jco-impl/webrtc.js:142-148`) and the Wasmtime `open_rx.await`
(`examples/wasmtime-demo/src/main.rs:174-176`) have no timeout; a failed
handshake hangs the guest's `open-echo` forever (the browser CI test would hit
the job-level timeout only). Add a timeout surfacing `error::timed-out` in both
hosts, plus listening to `connectionstatechange`/`iceconnectionstatechange`
`failed` states for fast failure.

### 18. Peer connections created by `openEcho` are never closed

`DataChannel.#keepAlive` retains `near`/`far`
(`jco-impl/webrtc.js:33-39,118`) but nothing ever calls `pc.close()`; under
`@roamhq/wrtc` this leaks native threads/sockets per channel. Mirror of item 9
for the JS host: close both peers when the channel closes/is dropped.

### 19. Remove unused `@bytecodealliance/componentize-js` devDependency

`jco-impl/package.json` declares `componentize-js` but the component is built
from Rust via `wasm-tools`; nothing in the repo invokes it. Dropping it shrinks
`npm install` (it pulls a large toolchain) — this is on the critical path of
`scripts/setup.sh` for every CI run and agent session.

## D. Development environment

### 23. Keeping `jco transpile` flags in sync with WIT is manual and error-prone

AGENTS.md documents that any interface rename must be mirrored in the
`--async-exports/--async-imports/--map` strings in `jco-impl/package.json`. Add
a CI check (or generate the flags from the WIT via a small script) so a rename
that misses the flags fails fast with a clear message rather than at
transpile/runtime.

## E. Examples

### 24. Dead demo WIT surface: `prompt`, `manual-demo`, and `browser-signaling-demo` reference a nonexistent `browser-signaling` component

`examples/cli-signaling/wit/webrtc-echo-demo.wit` defines `prompt`,
`manual-demo`, and the `browser-signaling-demo` world, and its docs (plus
`cli-signaling/src/lib.rs` docs) refer to a sibling "browser-signaling"
component "which speaks the exact same wire format" — but no such component or
jco host support exists, and the `cli-signaling` component doesn't export
`manual-demo` either (it exports `wasi:cli/run`). Either build the browser
manual-signaling demo (it would be the first exercise of the jco host beyond
echo) or delete the unused interfaces/worlds until it exists.

### 25. Guests read inbound streams with `Vec::with_capacity(count)` — worst case buffers the whole run in guest memory

`examples/echo-demo/src/lib.rs:71` (and the test guest) pass a buffer sized for
**all** `count` messages to each `read`, so a burst can materialize
`count × size` bytes in the guest (the demo defaults already allow 4 MiB;
larger params scale linearly). Read in small bounded batches to demonstrate the
memory-limiting pattern the interface is designed for — the examples are the
reference consumers.

### 26. Demos count bytes but never verify payload content or ordering

`make_message` tags each message with its index precisely so
ordering/integrity "could" be verified (`echo-demo/src/lib.rs:94-103`), but no
consumer checks it; the Wasmtime demo doesn't even assert `bytes_echoed`.
Verify content + order in the echo guest and the manual-signaling test so a
host that corrupts, reorders (with `ordered: true`), or duplicates messages
fails the demos.

### 27. Wire up `rendezvous` end-to-end: two-process real-signaling example (tracking)

The AGENTS.md-designated next step: implement the `rendezvous` host on both
stacks (Wasmtime via `wasi:http@0.3` outgoing handler per the
`wasm-component-starter` pattern; jco via `fetch`), pick/ship a trivial local
HTTP mailbox server, and add a guest that drives `signaling` (after item 8) +
`rendezvous` so two separate component instances (offerer/answerer) connect.
This would replace the `connect` shortcut as the flagship example and would
exercise nearly every interface at once — a good candidate for completely
replacing the existing examples.

### 28. No cross-host conformance story for edge-case behavior

The echo demo proves the happy path on both hosts, but divergences already
exist (item 1's receive-twice; typed errors, items 5/16; close semantics,
item 3). Define a small conformance guest (call `receive` twice, send after
close, zero-length message, oversized message, label round-trip) and run it
against both hosts in CI, asserting identical observable results.

### 29. Drive the sans-I/O `rtc` stack from inside a wasm guest (tracking)

`wasip3-impl` proves the wasm-capable
[`lann/rtc`](https://github.com/lann/rtc/tree/wasi) `wasi` fork interoperates
with `webrtc-rs` over a real DTLS + SCTP data channel, but the sans-I/O event
loop currently runs **host-side** in the native `NativePeer` driver
(`wasip3-impl/src/native.rs`). The runtime-agnostic `SansIoPeer`
(`wasip3-impl/src/peer.rs`) performs no I/O, so the natural next step is a
**guest** driver that feeds it from `wasi:sockets` (UDP) and WASI timers instead
of Tokio, then builds it for `wasm32-wasip2`. Host-candidate gathering must stay
explicit (`ifaces()` is `Unsupported` on wasm) — supply interface addresses via
config or use server-reflexive/relay candidates only. See the
`wasm-component-starter` `wasi:sockets`/timer patterns.

## Suggested priority

Correctness first (11), then interface-stabilizing decisions (5–7), then the
strategic items (8, 27, 28, 29); the rest are cheap hygiene wins
(14, 15, 19, 23, 24–26).
