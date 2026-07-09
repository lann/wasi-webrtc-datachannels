// Standalone self-test for the signaling server: exercises the full mailbox
// protocol (send / long-poll recv / role separation / close -> drain -> none)
// over real HTTP, with no WebRTC involved. Run with:
//
//     node signaling-server/selftest.mjs
//
// Exits 0 on success, 1 on any failed assertion.

import { startSignalingServer } from "./server.mjs";

function assert(cond, msg) {
  if (!cond) throw new Error(`assertion failed: ${msg}`);
}

async function main() {
  const { server, port } = await startSignalingServer(0);
  const base = `http://127.0.0.1:${port}`;
  const room = "selftest";

  const send = (role, text) =>
    fetch(`${base}/rooms/${room}/${role}/send`, { method: "POST", body: text });
  const recv = (role) => fetch(`${base}/rooms/${room}/${role}/recv`);
  const close = (role) => fetch(`${base}/rooms/${room}/${role}/close`, { method: "POST" });

  try {
    // 1. A blob published by the offerer is delivered to the answerer's recv,
    //    even when recv started polling *before* the blob was sent.
    const pending = recv("answerer");
    await send("offerer", "offer-sdp");
    const r1 = await pending;
    assert(r1.status === 200, `expected 200, got ${r1.status}`);
    assert((await r1.text()) === "offer-sdp", "answerer should receive offer-sdp");

    // 2. Ordering is FIFO within a direction.
    await send("offerer", "ice-1");
    await send("offerer", "ice-2");
    assert((await (await recv("answerer")).text()) === "ice-1", "first ICE");
    assert((await (await recv("answerer")).text()) === "ice-2", "second ICE");

    // 3. The reverse direction is independent (answerer -> offerer).
    await send("answerer", "answer-sdp");
    assert((await (await recv("offerer")).text()) === "answer-sdp", "offerer receives answer");

    // 4. After the peer closes and its queue drains, recv returns 204 (-> none).
    await send("offerer", "final");
    await close("offerer");
    assert((await (await recv("answerer")).text()) === "final", "drain remaining blob");
    const done = await recv("answerer");
    assert(done.status === 204, `expected 204 after close+drain, got ${done.status}`);

    console.log("signaling-server selftest: OK");
  } finally {
    server.close();
  }
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error("signaling-server selftest FAILED:", err);
    process.exit(1);
  },
);
