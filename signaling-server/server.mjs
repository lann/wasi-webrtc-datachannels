// A lightweight HTTP **long-polling** signaling server for the spike.
//
// WebRTC needs an out-of-band channel to exchange SDP offers/answers and
// trickled ICE candidates before a peer-to-peer connection can form. The usual
// choice is a WebSocket relay, but `wasi:http` has no WebSocket support (see the
// project README / AGENTS.md), so this server is built on plain HTTP
// request/response and **long-polling** instead — a transport every HTTP client
// can drive, whether native `fetch`/`reqwest` or a future `wasi:http@0.3` host.
//
// It is deliberately tiny and dependency-free (Node's built-in `http` only) so
// it can run locally and back the demo `rendezvous` mailbox. It is the concrete
// "existing HTTP signaling server" the two hosts relay blobs to and from.
//
// ## Model
//
// State is keyed by a `room` (a rendezvous session shared by exactly two peers)
// and a `role` (`offerer` or `answerer`). Each role owns an outbound queue of
// opaque blobs; a peer *reads the other role's* queue. This mirrors the
// `demo:webrtc-echo/rendezvous` WIT `session` resource exactly.
//
// ## Protocol
//
//   POST /rooms/{room}/{role}/send    body = opaque blob
//       Publish one blob for the peer to fetch. 204 on success.
//
//   GET  /rooms/{room}/{role}/recv
//       Fetch the next blob published by the *peer* (the other role),
//       long-polling until one is available. Responses:
//         200 + body : the next blob (in publish order).
//         204        : the peer closed its side and its queue is drained
//                      (maps to rendezvous `recv` -> `none`).
//         408        : the long-poll window elapsed with no blob and the peer
//                      still open; the client should simply poll again.
//
//   POST /rooms/{room}/{role}/close
//       Signal that this role has published its final blob. Idempotent. 204.
//
// `role` is the role of the *caller*. On `send`/`close` it identifies the queue
// being written; on `recv` the caller reads the opposite role's queue.

import { createServer } from "node:http";

/** How long a single `recv` long-poll is held open before returning 408. */
const LONG_POLL_MS = 25_000;

const OTHER = { offerer: "answerer", answerer: "offerer" };

/** Per-role mailbox: a FIFO of published blobs plus a closed flag. */
function newMailbox() {
  return { queue: [], closed: false, waiters: [] };
}

/** Lazily-created room holding one mailbox per role. */
function newRoom() {
  return { offerer: newMailbox(), answerer: newMailbox() };
}

export function createSignalingServer() {
  /** @type {Map<string, ReturnType<typeof newRoom>>} */
  const rooms = new Map();

  const room = (name) => {
    let r = rooms.get(name);
    if (!r) {
      r = newRoom();
      rooms.set(name, r);
    }
    return r;
  };

  // Wake every pending `recv` waiting on `mailbox` (a blob arrived or the
  // writer closed). Each waiter re-evaluates its queue/closed state.
  const wake = (mailbox) => {
    const waiters = mailbox.waiters;
    mailbox.waiters = [];
    for (const resolve of waiters) resolve();
  };

  const readBody = (req) =>
    new Promise((resolve, reject) => {
      const chunks = [];
      req.on("data", (c) => chunks.push(c));
      req.on("end", () => resolve(Buffer.concat(chunks)));
      req.on("error", reject);
    });

  const server = createServer(async (req, res) => {
    // Path shape: /rooms/{room}/{role}/{action}
    const url = new URL(req.url, "http://localhost");
    const parts = url.pathname.split("/").filter(Boolean);
    if (parts.length !== 4 || parts[0] !== "rooms") {
      res.writeHead(404).end();
      return;
    }
    const [, roomName, role, action] = parts.map((p) => decodeURIComponent(p));
    if (role !== "offerer" && role !== "answerer") {
      res.writeHead(400).end("unknown role");
      return;
    }

    const r = room(roomName);
    const mine = r[role];
    const theirs = r[OTHER[role]];

    try {
      if (action === "send" && req.method === "POST") {
        const blob = await readBody(req);
        mine.queue.push(blob);
        wake(mine); // wake the peer's recv, which reads `mine`.
        res.writeHead(204).end();
        return;
      }

      if (action === "close" && req.method === "POST") {
        mine.closed = true;
        wake(mine);
        res.writeHead(204).end();
        return;
      }

      if (action === "recv" && req.method === "GET") {
        await handleRecv(theirs, res, req);
        return;
      }

      res.writeHead(405).end();
    } catch (err) {
      res.writeHead(500).end(String(err?.message ?? err));
    }
  });

  // Long-poll `mailbox` for one blob. Reads `mailbox` (the peer's outbound
  // queue). Resolves the HTTP response with 200/204/408 per the protocol.
  function handleRecv(mailbox, res, req) {
    return new Promise((resolve) => {
      let settled = false;
      let timer;

      const finish = (fn) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        const i = mailbox.waiters.indexOf(check);
        if (i !== -1) mailbox.waiters.splice(i, 1);
        req.off("close", onAbort);
        fn();
        resolve();
      };

      const check = () => {
        if (mailbox.queue.length > 0) {
          const blob = mailbox.queue.shift();
          finish(() => {
            res.writeHead(200, { "content-type": "application/octet-stream" });
            res.end(blob);
          });
        } else if (mailbox.closed) {
          finish(() => res.writeHead(204).end());
        }
      };

      const onAbort = () => finish(() => {}); // client hung up; drop silently.

      // Fast path: data or close already pending.
      check();
      if (settled) return;

      // Slow path: register as a waiter and arm the long-poll timeout.
      mailbox.waiters.push(check);
      req.on("close", onAbort);
      timer = setTimeout(() => finish(() => res.writeHead(408).end()), LONG_POLL_MS);
    });
  }

  return server;
}

/** Start the server, returning a promise for the bound port. */
export function startSignalingServer(port = Number(process.env.SIGNALING_PORT) || 8787) {
  const server = createSignalingServer();
  return new Promise((resolve) => {
    server.listen(port, "127.0.0.1", () => {
      const addr = server.address();
      resolve({ server, port: typeof addr === "object" && addr ? addr.port : port });
    });
  });
}

// Run directly: `node signaling-server/server.mjs`.
if (import.meta.url === `file://${process.argv[1]}`) {
  startSignalingServer().then(({ port }) => {
    console.log(`signaling server (HTTP long-poll) listening on http://127.0.0.1:${port}`);
  });
}
