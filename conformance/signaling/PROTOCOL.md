# `conformance:signaling` mailbox protocol

This document specifies the wire protocol served by `conformance-signalingd`,
the suite-owned signaling server used by the conformance suite to relay opaque
signaling blobs (SDP offers/answers and trickled ICE candidates) between two
peers.

It is **test-only** infrastructure, deliberately separate from the demo
`rendezvous` proposal and from any future standardized signaling interface, so
it can evolve with the tests and be discarded without API cost.

## Design constraints

- **Plain HTTP/1.1 + long-poll only — no WebSockets.** The same protocol must be
  reachable identically from a native Rust client, a browser/Node `fetch`
  client, and an in-guest `wasi:http` client.
- **Opaque payloads.** The server never inspects blob contents. The conformance
  guest owns the blob encoding (JSON `session-description` / `ice-candidate`
  values with an explicit `end-of-candidates` message).
- **Idempotent, retry-safe reads.** Every fetch is addressed by an explicit
  sequence number, so a client may re-issue a fetch after a timeout, reconnect,
  or reload and observe the same result. This is required for flaky-network ICE
  scenarios and browser reloads.
- **In-memory, test-scoped.** No persistence, no auth. State is held in memory
  with a per-room TTL and request-size caps. The server binds an ephemeral
  localhost port by default; it can bind a routable address for cross-machine
  and NAT-lab runs.

## Model

The server holds a set of independent **rooms**, addressed by an opaque room id.
A room contains exactly two **mailboxes**, one per **role**:

- `offerer`
- `answerer`

Each mailbox is an append-only, ordered list of blobs plus a `done` flag. A peer
**publishes** blobs to *its own* role's mailbox and **fetches** blobs from *the
peer's* mailbox in publish order. Blobs within a mailbox are numbered by a
zero-based **sequence number** in publish order.

A room is created implicitly on first use (first publish or fetch that names
it). There is no registration step and no fixed association between a network
client and a role beyond the role named in each request.

## Roles and paths

`{room}` is a non-empty opaque token matching `[A-Za-z0-9._-]{1,128}`. `{role}`
and `{peer_role}` are exactly `offerer` or `answerer`.

All request and response bodies that carry a blob use
`Content-Type: application/octet-stream`; the blob is the raw body bytes. Status
and error responses use `Content-Type: application/json`.

## Endpoints

### `POST /rooms/{room}/{role}` — publish the next blob

Appends the request body as the next blob in `{role}`'s mailbox.

- Request body: raw blob bytes (`application/octet-stream`). An empty body is a
  valid zero-length blob.
- On success: `200 OK` with a JSON body `{ "seq": <n> }`, where `n` is the
  zero-based sequence number assigned to the published blob.
- If the mailbox has already been marked `done` (see below): `409 Conflict`
  with `{ "error": "done" }`. Publishing after `done` is a client error.
- If the body exceeds the configured size cap: `413 Payload Too Large` with
  `{ "error": "too-large", "limit": <bytes> }`.

Publishing is **not** idempotent: each `POST` appends a new blob. Publishers that
need retry-safety must track their own last-acked `seq` and avoid re-publishing.
(The conformance guest publishes a bounded, known sequence and does not retry
publishes; retry-safety is provided on the read side.)

### `GET /rooms/{room}/{peer_role}?seq={n}` — fetch blob `n` (long-poll)

Fetches blob number `n` from `{peer_role}`'s mailbox, long-polling until it is
available, the mailbox is marked `done` at or before `n`, or the poll deadline
elapses.

- `seq` is required and must be a non-negative integer. A missing or malformed
  `seq` yields `400 Bad Request` with `{ "error": "bad-seq" }`.
- Optional `wait={ms}` query parameter caps the long-poll duration for this
  request (milliseconds). Defaults to the server's configured long-poll timeout.
  `wait=0` makes the call non-blocking (immediate short-poll).
- Outcomes:
  - Blob `n` exists: `200 OK`, body is the raw blob bytes
    (`application/octet-stream`), header `X-Seq: n`.
  - The mailbox is marked `done` and `n` is at or past the end (i.e. blob `n`
    will never exist): `204 No Content` with header `X-Done: true`. This is the
    distinguished "no more blobs" response — it maps to `recv() -> none` in the
    guest WIT.
  - Blob `n` is not yet available and the mailbox is not done: the request
    blocks up to the wait deadline. If it arrives, respond as `200` above. If
    the deadline elapses first: `304 Not Modified` with header `X-Seq: n` and no
    body. `304` means "not yet; retry the same `seq`" — it is **not** an error
    and is safe to retry indefinitely.

Fetches are **idempotent**: refetching the same `{room}/{peer_role}?seq={n}`
always returns the same blob bytes once available. Sequence numbers may be
fetched in any order and refetched any number of times.

### `POST /rooms/{room}/{role}/done` — mark end-of-blobs

Marks `{role}`'s mailbox as `done`: no further blobs will be published. Any
blocked or subsequent fetch of a `seq` at or past the mailbox length resolves to
the `204`/`X-Done: true` response above.

- Request body: ignored (send empty).
- On success: `200 OK` with `{ "done": true, "len": <n> }`, where `len` is the
  number of blobs published to the mailbox at the time it was marked done.
- Marking an already-`done` mailbox is idempotent and returns `200` with the
  same shape.

`done` freezes the mailbox length; a fetch of any `seq < len` still returns its
blob, and any `seq >= len` returns the done response.

### `DELETE /rooms/{room}` — delete a room

Removes the room and both mailboxes immediately, waking any blocked fetches with
the done response.

- On success: `200 OK` with `{ "deleted": true }`.
- Deleting a nonexistent room is idempotent: `200 OK` with
  `{ "deleted": false }`.

### `GET /healthz` — readiness probe

Returns `200 OK` with body `ok` as soon as the server is accepting connections.
The runner polls this endpoint to gate startup before invoking adapters.

## Blob lifecycle summary

```
publish(role, blob)            -> seq n                (mailbox[role] grows)
done(role)                     -> len                  (mailbox[role] frozen)
fetch(peer_role, seq=n) ==
    n <  len(mailbox)          -> 200 blob             (idempotent)
    n >= len && !done          -> 304 (retry) / blocks until published
    n >= len && done           -> 204 X-Done: true     (=> recv -> none)
```

## Timeouts, TTL, and caps (server configuration)

- **Long-poll timeout** — default upper bound a `GET` blocks before returning
  `304`. Overridable per request via `wait={ms}`. Default: 25000 ms.
- **Room TTL** — a room is evicted this long after its last activity (any
  request touching it). Evicted rooms behave as nonexistent; a subsequent fetch
  creates a fresh empty room. Default: 300 s.
- **Blob size cap** — maximum publish body size; larger publishes get `413`.
  Default: 262144 bytes (256 KiB), comfortably above SDP + candidate sizes.
- **Room/blob-count caps** — optional coarse limits to bound memory in
  adversarial tests. Exceeding them yields `429 Too Many Requests` with
  `{ "error": "capacity" }`.

All defaults are overridable via CLI flags on `conformance-signalingd`.

## Error response shape

Non-2xx responses (except the bodyless `204`/`304`/`400`-with-header cases noted
above) carry a JSON body:

```json
{ "error": "<kebab-case-code>", "detail": "<optional human string>" }
```

Defined error codes: `bad-seq`, `bad-role`, `bad-room`, `done`, `too-large`,
`capacity`, `not-found`. Clients match on the `error` code, never on `detail`.

## Client guidance

- **Publisher:** `POST` each blob in order; on completion `POST .../done`.
- **Consumer:** loop `GET ...?seq=n` starting at `n = 0`; on `200` process the
  blob and increment `n`; on `304` retry the same `n` (optionally with backoff);
  on `204`/`X-Done` stop (peer is done). This loop is safe to restart from any
  `n` at any time.
- Long-poll is an optimization: a client that only short-polls (`wait=0`) with
  its own retry cadence observes identical results. The in-guest `wasi:http`
  client may use either mode.
