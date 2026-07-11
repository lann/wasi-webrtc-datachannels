// Host implementation of the `wasi:webrtc-data-channels` imports for Node.
//
// This is the "browser-first" host: it is written against the standard W3C
// WebRTC API (`RTCPeerConnection` / `RTCDataChannel`) and the WHATWG Streams
// API (`ReadableStream`), so the same logic runs in a browser. Under Node it is
// backed by `@roamhq/wrtc`, the maintained fork of `node-webrtc`, which
// provides those globals-compatible classes.
//
// `jco --map` wires this module in: the transpiled component does
//   import { openEcho } from '.../connect'      -> openEcho here
//   import { DataChannel } from '.../data-channels' -> DataChannel class here
//
// The component sees a channel already connected to an echo endpoint. Under the
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

/** The `data-channel` resource, implemented over an `RTCDataChannel`. */
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
   * Drain a stream of outbound messages into the channel. jco delivers the
   * `stream<list<u8>>` as an async iterable whose values are one `Uint8Array`
   * per `list<u8>` element, i.e. one data-channel message per iteration.
   * @param {AsyncIterable<Uint8Array>} messages
   */
  async send(messages) {
    for await (const message of messages) {
      await this.#waitForDrain();
      this.#channel.send(message);
    }
  }

  /** A stream of inbound messages, one chunk per received message. */
  async receive() {
    return this.#incoming;
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
 * Open a data channel connected to an internal echo endpoint.
 * @param {{ label: string, ordered: boolean, maxRetransmits?: number }} options
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

  const init = { ordered: options.ordered };
  if (options.maxRetransmits != null) {
    init.maxRetransmits = options.maxRetransmits;
  }
  const channel = near.createDataChannel(options.label, init);
  channel.binaryType = "arraybuffer";

  const incoming = incomingStream(channel);
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

/** Build a ReadableStream that yields one Uint8Array per inbound message. */
function incomingStream(channel) {
  return new ReadableStream({
    start(controller) {
      channel.addEventListener("message", ({ data }) => {
        controller.enqueue(new Uint8Array(data));
      });
      const end = () => {
        try {
          controller.close();
        } catch {
          // Already closed.
        }
      };
      channel.addEventListener("close", end);
      channel.addEventListener("error", end);
    },
  });
}

/** Resolve once the channel is open (or reject if it errors first). */
function waitOpen(channel) {
  if (channel.readyState === "open") return Promise.resolve();
  return new Promise((resolve, reject) => {
    channel.onopen = () => resolve();
    channel.onerror = (event) => reject(event.error ?? new Error("data channel error"));
  });
}
