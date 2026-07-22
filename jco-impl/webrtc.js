// Host implementation of the `lann:webrtc-datachannels` imports for Node.
//
// This is the "browser-first" host: it is written against the standard W3C
// WebRTC API (`RTCPeerConnection` / `RTCDataChannel`), so the same logic runs
// in a browser. Under Node it is backed by `@roamhq/wrtc`, the maintained fork
// of `node-webrtc`, which provides those globals-compatible classes.
//
// `jco --map` wires this module in: the transpiled component does
//   import { openEcho } from '.../connect'      -> openEcho here
//   import { DataChannel, DataChannelOptions } from '.../connections'
//                                               -> those classes here
//
// The guest builds a `DataChannelOptions` (a configuration builder) and passes
// it to `openEcho`. The component sees a channel already connected to an echo endpoint. Under the
// hood `openEcho` performs a genuine SDP offer/answer + ICE handshake between
// two peer connections and echoes every message on the far side, so a real
// WebRTC/SCTP data channel carries the traffic.

// Resolve `RTCPeerConnection` isomorphically so this exact module runs both in a
// real browser and under Node. In a browser (including headless Chromium in CI)
// the W3C class is a global; under Node it is provided by `@roamhq/wrtc`, which
// is imported lazily so the bare specifier never has to resolve in the browser.
const RTCPeerConnection =
  globalThis.RTCPeerConnection ??
  (await import("@roamhq/wrtc")).default.RTCPeerConnection;

// Keep the SCTP send buffer bounded; pause the producer when it fills.
const MAX_BUFFERED_AMOUNT = 8 * 1024 * 1024;

// The default bound on buffered inbound payload bytes awaiting `receive`.
// There is no wire-level inbound backpressure (the W3C API has no read-side
// flow control), so this bound is what protects memory from a slow guest
// reader: exceeding it closes the channel and, once the buffered backlog
// drains, `receive` fails with `error.receive-buffer-overflow`. Overridable —
// primarily as a test knob, so the conformance overflow probe needs only a
// small flood — through the `WEBRTC_MAX_INBOUND_BUFFER_BYTES` environment
// variable (Node) or a global of the same name (browsers).
const DEFAULT_MAX_INBOUND_BUFFERED = 8 * 1024 * 1024;

/** The configured inbound buffer bound, resolved lazily per channel. */
function maxInboundBuffered() {
  const configured = Number(
    globalThis.WEBRTC_MAX_INBOUND_BUFFER_BYTES ??
      globalThis.process?.env?.WEBRTC_MAX_INBOUND_BUFFER_BYTES,
  );
  return configured > 0 ? configured : DEFAULT_MAX_INBOUND_BUFFERED;
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
  // Retain the peer connections so they are not garbage-collected while in use.
  #keepAlive;

  constructor(channel, incoming, keepAlive) {
    this.#channel = channel;
    this.#incoming = incoming;
    this.#keepAlive = keepAlive;
  }

  /** The negotiated channel label. */
  label() {
    return this.#channel.label;
  }

  /**
   * Send a single message on the channel. jco delivers the `message` variant as
   * `{ tag: 'binary', val: Uint8Array }` or `{ tag: 'string', val: string }`.
   * @param {{ tag: 'binary', val: Uint8Array } | { tag: 'string', val: string }} message
   */
  async send(message) {
    await this.#waitForDrain();
    // A string is sent as a WebRTC text message; a Uint8Array as binary. Both
    // preserve the message kind end to end.
    this.#channel.send(message.val);
  }

  /**
   * Receive a single message from the channel, resolving with the next inbound
   * `message` variant or throwing `{ tag: 'closed' }` once the channel closes.
   */
  async receive() {
    return this.#incoming.next();
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
 * The `data-channel-options` resource: a configuration builder for a data
 * channel, following the shape of `wasi:http`'s `request-options`. The guest
 * constructs a default value, adjusts fields through the setters, and hands it
 * to `openEcho`. Each field has a getter/setter accessor pair.
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
}

/**
 * Open a data channel connected to an internal echo endpoint.
 * @param {DataChannelOptions} options
 * @returns {Promise<DataChannel>}
 */
export async function openEcho(options) {
  const near = new RTCPeerConnection();
  const far = new RTCPeerConnection();

  // Trickle ICE candidates directly between the two local peers.
  near.onicecandidate = ({ candidate }) => candidate && far.addIceCandidate(candidate);
  far.onicecandidate = ({ candidate }) => candidate && near.addIceCandidate(candidate);

  // Far side: echo every message straight back on the same channel.
  far.ondatachannel = ({ channel }) => {
    channel.binaryType = "arraybuffer";
    channel.onmessage = ({ data }) => channel.send(data);
  };

  const init = { ordered: options.ordered() };
  const maxRetransmits = options.maxRetransmits();
  if (maxRetransmits != null) {
    init.maxRetransmits = maxRetransmits;
  }
  const channel = near.createDataChannel(options.label(), init);
  channel.binaryType = "arraybuffer";

  const incoming = incomingQueue(channel);
  const opened = waitOpen(channel);

  // Standard offer/answer exchange.
  const offer = await near.createOffer();
  await near.setLocalDescription(offer);
  await far.setRemoteDescription(offer);
  const answer = await far.createAnswer();
  await far.setLocalDescription(answer);
  await near.setRemoteDescription(answer);

  await opened;
  return new DataChannel(channel, incoming, { near, far });
}

/**
 * Build a per-message inbound queue over `channel`. Each received message is
 * tagged as a `message` variant (`{ tag: 'binary', val: Uint8Array }` for binary
 * frames, `{ tag: 'string', val: string }` for text frames). `next()` resolves
 * with the next message, or rejects with `{ tag: 'closed' }` once the channel
 * closes with no more messages pending.
 *
 * Buffering is bounded by `maxInboundBuffered()` payload bytes: a message that
 * would exceed it closes the channel and discards that and any later messages;
 * the pre-overflow backlog stays deliverable, after which `next()` rejects with
 * `{ tag: 'receive-buffer-overflow' }`.
 */
function incomingQueue(channel) {
  const limit = maxInboundBuffered();
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
    if (buffered + size > limit && !waiters.length) {
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

/** Resolve once the channel is open (or reject if it errors first). */
function waitOpen(channel) {
  if (channel.readyState === "open") return Promise.resolve();
  return new Promise((resolve, reject) => {
    channel.onopen = () => resolve();
    channel.onerror = (event) => reject(event.error ?? new Error("data channel error"));
  });
}
