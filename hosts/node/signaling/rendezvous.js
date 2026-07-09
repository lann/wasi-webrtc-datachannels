// Node host implementation of the `demo:webrtc-echo/rendezvous` mailbox.
//
// `rendezvous` is how two *separate* peers exchange opaque SDP/ICE blobs before
// a direct WebRTC connection exists. This host relays those blobs to and from
// the lightweight HTTP long-polling signaling server in `signaling-server/`
// using `fetch` — the browser-first analogue of a `wasi:http@0.3` client. The
// guest never speaks HTTP itself; it only sees the `session` resource.
//
// `jco --map demo:webrtc-echo/rendezvous@0.1.0=...` wires this module in. The
// `Session` class below implements the WIT `session` resource: `open` (static),
// `send`, `recv`, and `close`.
//
// The server base URL is taken from `SIGNALING_URL` (default
// `http://127.0.0.1:8787`).

const BASE_URL = () => process.env.SIGNALING_URL ?? "http://127.0.0.1:8787";

/** The `session` resource: one peer's handle on a shared rendezvous room. */
export class Session {
  #base;
  #room;
  #role;

  constructor(base, room, role) {
    this.#base = base;
    this.#room = room;
    this.#role = role;
  }

  /**
   * Join (creating if necessary) `room` on the signaling server as `role`.
   * @param {string} room
   * @param {'offerer' | 'answerer'} role
   */
  static async open(room, role) {
    // Nothing to do server-side until the first send/recv: rooms are created
    // lazily. Returning the handle keeps `open` cheap and offline-tolerant.
    return new Session(BASE_URL(), room, role);
  }

  #url(action) {
    return `${this.#base}/rooms/${encodeURIComponent(this.#room)}/${this.#role}/${action}`;
  }

  /**
   * Publish the next opaque signaling blob for the peer to fetch.
   * @param {Uint8Array} blob
   */
  async send(blob) {
    const res = await fetch(this.#url("send"), { method: "POST", body: blob });
    if (!res.ok) {
      throw new Error(`rendezvous send failed: HTTP ${res.status}`);
    }
  }

  /**
   * Fetch the next blob published by the peer, long-polling until one is
   * available. Returns the bytes for `some`, or `undefined` for `none` once the
   * peer has closed its side and its queue is drained.
   * @returns {Promise<Uint8Array | undefined>}
   */
  async recv() {
    for (;;) {
      const res = await fetch(this.#url("recv"));
      if (res.status === 200) {
        return new Uint8Array(await res.arrayBuffer());
      }
      if (res.status === 204) {
        return undefined; // peer closed and drained -> `none`.
      }
      if (res.status === 408) {
        continue; // long-poll window elapsed; poll again.
      }
      throw new Error(`rendezvous recv failed: HTTP ${res.status}`);
    }
  }

  /**
   * Signal to the peer that this side has published its final blob. Sync per the
   * WIT `close: func()`, so the HTTP request is best-effort fire-and-forget.
   */
  close() {
    fetch(this.#url("close"), { method: "POST" }).catch(() => {});
  }
}
