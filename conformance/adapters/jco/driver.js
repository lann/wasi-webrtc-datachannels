// Environment-agnostic conformance corpus driver shared by the Node and browser
// jco runners. Given a factory that produces fresh guest instances and the base
// URL of a signaling server, it runs each registered test to a raw
// `pass`/`fail`/`skip` result and returns the adapter result rows.
//
// It mirrors the wasmtime adapter's orchestration (`conformance/adapters/
// wasmtime/src/main.rs`): a test is run either as a single in-process `both`
// instance (peer-connection API + invalid-signaling probes), a single instance
// the guest reports `skipped` regardless of role (streaming + remaining
// error-taxonomy tests), or two instances — an offerer and an answerer sharing
// one signaling room — for the behavioral/interop tests. Two flaky loopback-ICE
// handshakes are retried with fresh rooms before a failure is reported. The
// guest owns every assertion; the driver only orchestrates and records.

/** The registry of test ids, mirroring `conformance/tests.toml`. */
export const TESTS = [
  "label-round-trip",
  "binary-message",
  "text-message",
  "message-boundaries",
  "zero-length-message",
  "large-message",
  "ordering",
  "payload-integrity",
  "concurrent-send-receive",
  "send-via-stream",
  "receive-via-stream",
  "receive-via-stream-once",
  "post-close-send",
  "max-retransmits-accepted",
  "error-invalid-signaling",
  "error-closed",
  "error-timed-out",
  "peer-offer-answer",
  "peer-create-data-channel",
  "peer-local-ice-candidates",
  "peer-add-ice-candidate",
  "peer-wait-connected",
  "peer-close-releases",
  "peer-invalid-sdp",
  "interop-handshake",
];

// How a test is orchestrated across guest instances.
const IN_PROCESS = new Set([
  "peer-offer-answer",
  "peer-create-data-channel",
  "peer-local-ice-candidates",
  "peer-add-ice-candidate",
  "peer-wait-connected",
  "peer-close-releases",
  "peer-invalid-sdp",
  "error-invalid-signaling",
]);
const SKIP = new Set([
  "send-via-stream",
  "receive-via-stream",
  "receive-via-stream-once",
  "post-close-send",
  "error-closed",
  "error-timed-out",
]);

/** The orchestration plan for a test id: `in-process`, `skip`, or `two-peer`. */
export function planFor(testId) {
  if (IN_PROCESS.has(testId)) return "in-process";
  if (SKIP.has(testId)) return "skip";
  return "two-peer";
}

/** The `[messageCount, messageSize]` a test runs with. */
export function paramsFor(testId) {
  switch (testId) {
    case "large-message":
      return [1, 16384];
    case "message-boundaries":
    case "ordering":
    case "payload-integrity":
    case "concurrent-send-receive":
    case "interop-handshake":
      return [16, 512];
    default:
      return [4, 256];
  }
}

// The number of connection attempts before a flaky handshake is reported as a
// failure. Each attempt uses fresh peer connections and a fresh room.
const MAX_ATTEMPTS = 3;

// How long a single attempt may run before it is abandoned as a stalled
// handshake and retried, bounding an attempt whose data-channel wait never
// resolves.
const ATTEMPT_TIMEOUT_MS = 45_000;

/** Whether a failure detail looks like a retryable loopback-ICE flake. */
function isFlaky(detail) {
  return detail.includes("timed-out") || detail.includes("wait-connected");
}

/** Build a test config for one instance. */
function makeConfig(role, base, room, count, size) {
  return {
    role,
    signalingServer: base,
    room,
    messageCount: count,
    messageSize: size,
    trickle: true,
  };
}

/** Reject `promise` if it has not settled within `ms` milliseconds. */
function withTimeout(promise, ms, message) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(message)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/**
 * Fold two per-instance results into one: any fail loses, else any skip, else
 * pass. Mirrors the wasmtime adapter's `fold_two`.
 */
function foldTwo(offerer, answerer) {
  if (offerer.tag === "fail" && answerer.tag === "fail") {
    return { tag: "fail", val: `offerer: ${offerer.val}; answerer: ${answerer.val}` };
  }
  if (offerer.tag === "fail") return { tag: "fail", val: `offerer: ${offerer.val}` };
  if (answerer.tag === "fail") return { tag: "fail", val: `answerer: ${answerer.val}` };
  if (offerer.tag === "skipped") return offerer;
  if (answerer.tag === "skipped") return answerer;
  return { tag: "pass" };
}

/** Run one guest instance to a `test-result`. */
async function runInstance(newInstance, testId, config) {
  const instance = await newInstance();
  return instance.runner.runTest(testId, config);
}

/**
 * Run one test to a raw result, retrying flaky handshakes with fresh rooms.
 * `roomSeq` is a mutable `{ n }` counter shared across the corpus so rooms never
 * collide between concurrent tests.
 */
async function runTest(newInstance, base, testId, roomSeq) {
  const [count, size] = paramsFor(testId);
  const plan = planFor(testId);
  let lastDetail = null;

  for (let attempt = 0; attempt < MAX_ATTEMPTS; attempt++) {
    const room = `conf-${testId}-${roomSeq.n++}`;
    let result;
    try {
      const run = (async () => {
        switch (plan) {
          case "two-peer": {
            const [offerer, answerer] = await Promise.all([
              runInstance(newInstance, testId, makeConfig("offerer", base, room, count, size)),
              runInstance(newInstance, testId, makeConfig("answerer", base, room, count, size)),
            ]);
            return foldTwo(offerer, answerer);
          }
          case "in-process":
            return runInstance(newInstance, testId, makeConfig("both", base, room, count, size));
          default: // skip
            return runInstance(newInstance, testId, makeConfig("offerer", base, room, count, size));
        }
      })();
      result = await withTimeout(run, ATTEMPT_TIMEOUT_MS, "attempt timed-out");
    } catch (err) {
      // A stalled attempt or a host/adapter error: retry with a fresh room.
      lastDetail = String(err && err.message ? err.message : err);
      if (lastDetail !== "attempt timed-out") break;
      continue;
    }

    if (result.tag === "pass") {
      return { test_id: testId, status: "pass" };
    }
    if (result.tag === "skipped") {
      return { test_id: testId, status: "skip", detail: result.val };
    }
    // fail
    lastDetail = result.val;
    if (!isFlaky(result.val)) break;
  }

  return { test_id: testId, status: "fail", detail: lastDetail };
}

/**
 * Run the corpus and return the adapter result rows.
 * @param {object} opts
 * @param {string} opts.base signaling server base URL
 * @param {() => Promise<{ runner: { runTest: Function } }>} opts.newInstance
 *   factory producing a fresh guest instance
 * @param {string[]} [opts.only] run only these test ids (empty => all)
 * @param {(msg: string) => void} [opts.log] progress logger
 * @returns {Promise<Array<{ test_id: string, status: string, detail?: string }>>}
 */
export async function runCorpus({ base, newInstance, only = [], log = () => {} }) {
  const roomSeq = { n: 0 };
  const results = [];
  for (const testId of TESTS) {
    if (only.length && !only.includes(testId)) continue;
    log(`running ${testId} … `);
    const result = await runTest(newInstance, base, testId, roomSeq);
    log(`${result.status}\n`);
    results.push(result);
  }
  return results;
}
