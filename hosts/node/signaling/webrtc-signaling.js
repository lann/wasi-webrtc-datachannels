// Node host implementation of the `wasi:webrtc-data-channels/signaling` and
// `data-channels` imports for the real-signaling demo.
//
// This is the "browser-first" host: it is written against the standard W3C
// WebRTC API (`RTCPeerConnection` / `RTCDataChannel`) and WHATWG Streams
// (`ReadableStream`), so the same logic would run in a browser. Under Node it is
// backed by `@roamhq/wrtc`.
//
// `jco --map` wires this module in for both the `signaling` interface (the
// `PeerConnection` resource the guest drives) and the `data-channels` interface
// (the `DataChannel` resource that `create-data-channel` / `incoming-data-
// channels` hand back). Unlike the echo host, no host-internal peer or echo
// endpoint is involved: the guest performs a genuine offer/answer + trickle-ICE
// handshake with a *separate* peer, relayed through the `rendezvous` mailbox.

import pkg from "@roamhq/wrtc";
const { RTCPeerConnection } = pkg;

// Keep the SCTP send buffer bounded; pause the producer when it fills.
const MAX_BUFFERED_AMOUNT = 8 * 1024 * 1024;

/** The `data-channel` resource, implemented over an `RTCDataChannel`. */
export class DataChannel {
  #channel;
  #incoming;
  // Retain the peer connection so it is not garbage-collected while in use.
  #keepAlive;

  constructor(channel, keepAlive) {
    this.#channel = channel;
    channel.binaryType = "arraybuffer";
    this.#incoming = incomingStream(channel);
    this.#keepAlive = keepAlive;
  }

  /** The negotiated channel label. */
  label() {
    return this.#channel.label;
  }

  /**
   * Drain a stream of outbound messages into the channel. jco delivers the
   * `stream<list<u8>>` as an async iterable of one `Uint8Array` per message.
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

/** The `peer-connection` resource, implemented over an `RTCPeerConnection`. */
export class PeerConnection {
  #pc;
  // Buffer ICE candidates and incoming data channels from construction, so no
  // event is lost if the guest attaches its stream after gathering/negotiation
  // has already begun.
  #ice = new AsyncQueue();
  #channels = new AsyncQueue();

  constructor() {
    const pc = new RTCPeerConnection();
    this.#pc = pc;
    pc.addEventListener("icecandidate", ({ candidate }) => {
      // A non-null candidate with a non-empty string is a real one to trickle;
      // a `null` candidate marks end-of-gathering. We keep the stream open
      // either way (the guest stops reading once connected), which also avoids
      // races where a late candidate arrives after end-of-gathering.
      if (candidate && candidate.candidate) {
        this.#ice.push({
          candidate: candidate.candidate,
          sdpMid: candidate.sdpMid ?? undefined,
          sdpMlineIndex: candidate.sdpMLineIndex ?? undefined,
        });
      }
    });
    pc.addEventListener("datachannel", ({ channel }) => {
      this.#channels.push(new DataChannel(channel, pc));
    });
  }

  /** Create a data channel to be negotiated in-band with the peer. */
  createDataChannel(options) {
    const init = { ordered: options.ordered };
    if (options.maxRetransmits != null) {
      init.maxRetransmits = options.maxRetransmits;
    }
    const channel = this.#pc.createDataChannel(options.label, init);
    return new DataChannel(channel, this.#pc);
  }

  /** A stream of data channels opened by the remote peer. */
  incomingDataChannels() {
    return this.#channels.toStream();
  }

  async createOffer() {
    return descriptionOut(await this.#pc.createOffer());
  }

  async createAnswer() {
    return descriptionOut(await this.#pc.createAnswer());
  }

  async setLocalDescription(description) {
    await this.#pc.setLocalDescription(descriptionIn(description));
  }

  async setRemoteDescription(description) {
    await this.#pc.setRemoteDescription(descriptionIn(description));
  }

  /** A stream of locally gathered ICE candidates to trickle to the peer. */
  localIceCandidates() {
    return this.#ice.toStream();
  }

  async addIceCandidate(candidate) {
    await this.#pc.addIceCandidate({
      candidate: candidate.candidate,
      sdpMid: candidate.sdpMid ?? null,
      sdpMLineIndex: candidate.sdpMlineIndex ?? null,
    });
  }

  /** Resolve once the connection reaches the `connected` state. */
  waitConnected() {
    const pc = this.#pc;
    return new Promise((resolve, reject) => {
      const check = () => {
        const state = pc.connectionState;
        if (state === "connected") {
          cleanup();
          resolve();
        } else if (state === "failed" || state === "closed") {
          cleanup();
          reject(new Error(`peer connection ${state}`));
        }
      };
      const cleanup = () => pc.removeEventListener("connectionstatechange", check);
      pc.addEventListener("connectionstatechange", check);
      check();
    });
  }

  close() {
    this.#pc.close();
  }
}

/** Map a WIT `session-description` to a W3C `RTCSessionDescriptionInit`. */
function descriptionIn(description) {
  return { type: description.kind, sdp: description.sdp };
}

/** Map a W3C session description to a WIT `session-description`. */
function descriptionOut(description) {
  return { kind: description.type, sdp: description.sdp };
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

/**
 * A minimal push/pull queue that buffers items until consumed, exposed as a
 * `ReadableStream`. Used to capture ICE candidates and incoming data channels
 * from construction so none are lost before the guest reads them.
 */
class AsyncQueue {
  #items = [];
  #pending = [];
  #closed = false;

  push(item) {
    if (this.#closed) return;
    const resolve = this.#pending.shift();
    if (resolve) resolve({ value: item, done: false });
    else this.#items.push(item);
  }

  close() {
    this.#closed = true;
    let resolve;
    while ((resolve = this.#pending.shift())) {
      resolve({ value: undefined, done: true });
    }
  }

  #next() {
    if (this.#items.length > 0) {
      return Promise.resolve({ value: this.#items.shift(), done: false });
    }
    if (this.#closed) {
      return Promise.resolve({ value: undefined, done: true });
    }
    return new Promise((resolve) => this.#pending.push(resolve));
  }

  toStream() {
    const queue = this;
    return new ReadableStream({
      async pull(controller) {
        const { value, done } = await queue.#next();
        if (done) controller.close();
        else controller.enqueue(value);
      },
    });
  }
}
