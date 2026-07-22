// The `lann:webrtc-datachannels/connections` host for the jco conformance
// adapters, written against the standard W3C WebRTC API so the exact same module
// runs under Node (backed by `@roamhq/wrtc`) and in a real browser (where the
// classes are globals). It is the conformance counterpart of `jco-impl/webrtc.js`
// — that module implements only the demo `connect` shortcut, whereas this one
// implements the full `connections` surface the conformance guest drives:
// `data-channel-options`, `data-channel`, and `peer-connection` (offer/answer,
// trickle ICE, incoming channels).
//
// jco wires this module in as the component's `connections` import (via
// `instantiate(..., { 'lann:webrtc-datachannels/connections': <this module> })`).
// Errors are surfaced to the guest by throwing the WIT `error` variant value
// (for example `{ tag: 'closed' }` or `{ tag: 'invalid-signaling', val }`), which
// jco lifts into the `result<_, error>` the WIT declares.

// Resolve `RTCPeerConnection` isomorphically: a browser (including headless
// Chromium) exposes the W3C class as a global; under Node it is provided by
// `@roamhq/wrtc`, imported lazily so the bare specifier never has to resolve in
// the browser. A missing Node dependency is surfaced with an actionable message
// rather than a bare module-resolution error.
async function resolveRTCPeerConnection() {
  if (globalThis.RTCPeerConnection) return globalThis.RTCPeerConnection;
  try {
    return (await import("@roamhq/wrtc")).default.RTCPeerConnection;
  } catch (cause) {
    throw new Error(
      "no RTCPeerConnection available: not running in a browser and @roamhq/wrtc " +
        "could not be loaded (run `npm install` in conformance/adapters/jco)",
      { cause },
    );
  }
}

const RTCPeerConnection = await resolveRTCPeerConnection();

// Keep the SCTP send buffer bounded; pause the producer when it fills.
const MAX_BUFFERED_AMOUNT = 8 * 1024 * 1024;

// The bound on buffered inbound payload bytes awaiting `receive`. There is no
// wire-level inbound backpressure (the W3C API has no read-side flow control),
// so this bound is what protects memory from a slow guest reader: exceeding it
// closes the channel and, once the buffered backlog drains, `receive` fails
// with `error.receive-buffer-overflow`.
const MAX_INBOUND_BUFFERED = 8 * 1024 * 1024;

/**
 * The `data-channel-options` resource: a configuration builder for a data
 * channel, mirroring `wasi:http`'s `request-options`. The guest constructs a
 * default value, adjusts fields through the setters, and hands it to
 * `peer-connection.create-data-channel`.
 */
export class DataChannelOptions {
  #label = "";
  #ordered = true;
  #maxRetransmits = undefined;

  /** The channel label. */
  label() {
    return this.#label;
  }
  /** @param {string} label */
  setLabel(label) {
    this.#label = label;
  }

  /** Whether messages are delivered in order. */
  ordered() {
    return this.#ordered;
  }
  /** @param {boolean} ordered */
  setOrdered(ordered) {
    this.#ordered = ordered;
  }

  /** The maximum number of retransmissions, or `undefined` for reliable delivery. */
  maxRetransmits() {
    return this.#maxRetransmits;
  }
  /** @param {number | undefined} maxRetransmits */
  setMaxRetransmits(maxRetransmits) {
    this.#maxRetransmits = maxRetransmits;
  }

  /** The `RTCDataChannelInit` these options describe. */
  toInit() {
    const init = { ordered: this.#ordered };
    if (this.#maxRetransmits != null) {
      init.maxRetransmits = this.#maxRetransmits;
    }
    return init;
  }
}

/**
 * The `data-channel` resource, implemented over an `RTCDataChannel`.
 *
 * `send`/`receive` each carry exactly one data-channel message, preserving
 * WebRTC message boundaries. A message is a variant: `{ tag: 'binary', val:
 * Uint8Array }` or `{ tag: 'string', val: string }`.
 */
export class DataChannel {
  #channel;
  #incoming;

  constructor(channel) {
    this.#channel = channel;
    channel.binaryType = "arraybuffer";
    this.#incoming = incomingQueue(channel);
  }

  /** The negotiated channel label. */
  label() {
    return this.#channel.label;
  }

  /**
   * Send a single message on the channel, resolving once it has been handed to
   * the transport or rejecting with `{ tag: 'closed' }` if the channel closed.
   * @param {{ tag: 'binary', val: Uint8Array } | { tag: 'string', val: string }} message
   */
  async send(message) {
    await this.#waitOpen();
    await this.#waitForDrain();
    try {
      this.#channel.send(message.val);
    } catch {
      throw { tag: "closed" };
    }
  }

  /**
   * Receive a single message, resolving with the next inbound `message` variant
   * or rejecting with `{ tag: 'closed' }` once the channel closes.
   */
  async receive() {
    return this.#incoming.next();
  }

  /** Resolve once the channel is open, or reject `{ tag: 'closed' }` if it closes. */
  #waitOpen() {
    const channel = this.#channel;
    if (channel.readyState === "open") return Promise.resolve();
    if (channel.readyState === "closing" || channel.readyState === "closed") {
      return Promise.reject({ tag: "closed" });
    }
    return new Promise((resolve, reject) => {
      channel.addEventListener("open", () => resolve(), { once: true });
      channel.addEventListener("close", () => reject({ tag: "closed" }), { once: true });
      channel.addEventListener("error", () => reject({ tag: "closed" }), { once: true });
    });
  }

  /** Apply backpressure so a fast producer cannot overrun the SCTP buffer. */
  #waitForDrain() {
    const channel = this.#channel;
    if (channel.bufferedAmount <= MAX_BUFFERED_AMOUNT) return Promise.resolve();
    return new Promise((resolve) => {
      channel.bufferedAmountLowThreshold = MAX_BUFFERED_AMOUNT / 2;
      const onLow = () => {
        channel.removeEventListener("bufferedamountlow", onLow);
        resolve();
      };
      channel.addEventListener("bufferedamountlow", onLow);
    });
  }
}

/**
 * A single WebRTC peer connection driving the full `RTCPeerConnection`-style
 * signaling surface: offer/answer, trickle ICE, and in-band data channels.
 */
export class PeerConnection {
  #pc;
  #candidates;
  #channels;

  constructor() {
    this.#pc = new RTCPeerConnection();

    // Local ICE candidates: a `null` (or empty) candidate ends the stream.
    this.#candidates = eventStream((push, end) => {
      this.#pc.addEventListener("icecandidate", ({ candidate }) => {
        if (candidate == null || candidate.candidate === "") {
          end();
          return;
        }
        push({
          candidate: candidate.candidate,
          sdpMid: candidate.sdpMid ?? undefined,
          sdpMlineIndex: candidate.sdpMLineIndex ?? undefined,
        });
      });
    });

    // Data channels opened by the remote peer.
    this.#channels = eventStream((push) => {
      this.#pc.addEventListener("datachannel", ({ channel }) => {
        push(new DataChannel(channel));
      });
    });
  }

  /**
   * Create a data channel negotiated in-band with the peer.
   * @param {DataChannelOptions} options
   */
  createDataChannel(options) {
    const channel = this.#pc.createDataChannel(options.label(), options.toInit());
    return new DataChannel(channel);
  }

  /** A stream of data channels opened by the remote peer. */
  incomingDataChannels() {
    return this.#channels.stream;
  }

  /** Produce an SDP offer describing the local peer. */
  async createOffer() {
    const offer = await this.#pc.createOffer();
    return { kind: "offer", sdp: offer.sdp };
  }

  /** Produce an SDP answer in response to a previously set remote offer. */
  async createAnswer() {
    const answer = await this.#pc.createAnswer();
    return { kind: "answer", sdp: answer.sdp };
  }

  /**
   * Apply a local description produced by `createOffer`/`createAnswer`.
   * @param {{ kind: string, sdp: string }} description
   */
  async setLocalDescription(description) {
    try {
      await this.#pc.setLocalDescription({ type: description.kind, sdp: description.sdp });
    } catch (err) {
      throw { tag: "invalid-signaling", val: String(err) };
    }
  }

  /**
   * Apply the remote peer's description.
   * @param {{ kind: string, sdp: string }} description
   */
  async setRemoteDescription(description) {
    try {
      await this.#pc.setRemoteDescription({ type: description.kind, sdp: description.sdp });
    } catch (err) {
      throw { tag: "invalid-signaling", val: String(err) };
    }
  }

  /** A stream of locally gathered ICE candidates to trickle to the peer. */
  localIceCandidates() {
    return this.#candidates.stream;
  }

  /**
   * Add an ICE candidate received from the remote peer.
   * @param {{ candidate: string, sdpMid?: string, sdpMlineIndex?: number }} candidate
   */
  async addIceCandidate(candidate) {
    try {
      await this.#pc.addIceCandidate({
        candidate: candidate.candidate,
        sdpMid: candidate.sdpMid ?? null,
        sdpMLineIndex: candidate.sdpMlineIndex ?? null,
      });
    } catch (err) {
      throw { tag: "invalid-signaling", val: String(err) };
    }
  }

  /** Resolve once the connection reaches `connected`, or reject `{ tag: 'timed-out' }`. */
  async waitConnected() {
    const pc = this.#pc;
    const isConnected = () =>
      pc.connectionState === "connected" ||
      pc.iceConnectionState === "connected" ||
      pc.iceConnectionState === "completed";
    const isFailed = () => pc.connectionState === "failed" || pc.iceConnectionState === "failed";

    if (isConnected()) return;
    await new Promise((resolve, reject) => {
      const check = () => {
        if (isConnected()) {
          cleanup();
          resolve();
        } else if (isFailed()) {
          cleanup();
          reject({ tag: "timed-out" });
        }
      };
      const cleanup = () => {
        pc.removeEventListener("connectionstatechange", check);
        pc.removeEventListener("iceconnectionstatechange", check);
      };
      pc.addEventListener("connectionstatechange", check);
      pc.addEventListener("iceconnectionstatechange", check);
    });
  }

  /** Close the peer connection and any of its data channels. */
  close() {
    this.#pc.close();
  }
}

/**
 * A `ReadableStream` fed by an event source. `setup(push, end)` wires the source
 * to `push` each value and `end` to close the stream; values pushed before the
 * stream starts pulling are buffered.
 */
function eventStream(setup) {
  let controller;
  let ended = false;
  const buffer = [];
  const stream = new ReadableStream({
    start(c) {
      controller = c;
      for (const item of buffer) c.enqueue(item);
      buffer.length = 0;
      if (ended) c.close();
    },
  });
  const push = (item) => {
    if (controller) controller.enqueue(item);
    else buffer.push(item);
  };
  const end = () => {
    if (ended) return;
    ended = true;
    if (controller) {
      try {
        controller.close();
      } catch {
        // Already closed.
      }
    }
  };
  setup(push, end);
  return { stream };
}

/**
 * Build a per-message inbound queue over `channel`. Each received message is
 * tagged as a `message` variant (`{ tag: 'binary', val: Uint8Array }` for binary
 * frames, `{ tag: 'string', val: string }` for text frames). `next()` resolves
 * with the next message, or rejects with `{ tag: 'closed' }` once the channel
 * closes with no more messages pending.
 *
 * Buffering is bounded by `MAX_INBOUND_BUFFERED` payload bytes: a message that
 * would exceed it closes the channel and discards that and any later messages;
 * the pre-overflow backlog stays deliverable, after which `next()` rejects with
 * `{ tag: 'receive-buffer-overflow' }`.
 */
function incomingQueue(channel) {
  const messages = [];
  const waiters = [];
  let buffered = 0;
  let overflowed = false;
  let closed = false;

  const push = (message, size) => {
    const waiter = waiters.shift();
    if (waiter) {
      waiter.resolve(message);
    } else {
      buffered += size;
      messages.push({ message, size });
    }
  };

  channel.addEventListener("message", ({ data }) => {
    if (overflowed) return;
    const size = typeof data === "string" ? data.length : data.byteLength;
    if (buffered + size > MAX_INBOUND_BUFFERED && !waiters.length) {
      // The bounded inbound buffer overflowed: close the channel and discard
      // this and any later messages. Already-buffered messages stay deliverable.
      overflowed = true;
      channel.close();
      return;
    }
    const message =
      typeof data === "string"
        ? { tag: "string", val: data }
        : { tag: "binary", val: new Uint8Array(data) };
    push(message, size);
  });

  const endError = () => (overflowed ? { tag: "receive-buffer-overflow" } : { tag: "closed" });
  const end = () => {
    if (closed) return;
    closed = true;
    while (waiters.length) {
      waiters.shift().reject(endError());
    }
  };
  channel.addEventListener("close", end);
  channel.addEventListener("error", end);

  return {
    next() {
      if (messages.length) {
        const { message, size } = messages.shift();
        buffered -= size;
        return Promise.resolve(message);
      }
      if (overflowed) return Promise.reject({ tag: "receive-buffer-overflow" });
      if (closed) return Promise.reject({ tag: "closed" });
      return new Promise((resolve, reject) => waiters.push({ resolve, reject }));
    },
  };
}
