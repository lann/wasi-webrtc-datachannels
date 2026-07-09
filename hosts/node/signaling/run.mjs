// Driver for the Node real-signaling demo. It:
//
//   1. starts the lightweight HTTP long-polling signaling server in-process,
//   2. forks two independent peer processes — an `offerer` and an `answerer` —
//      pointed at the same rendezvous room, and
//   3. waits for both to finish and asserts a complete round trip.
//
// The two peers share nothing but the signaling server: they exchange SDP and
// ICE through the rendezvous room and then talk directly over a real
// WebRTC/SCTP data channel. This is the "separate peers" counterpart to the
// host-internal `connect` echo demo.
//
// Run with:  npm run signaling
import { fork } from "node:child_process";
import { fileURLToPath } from "node:url";
import { startSignalingServer } from "../../../signaling-server/server.mjs";

const MESSAGE_COUNT = Number(process.env.MESSAGE_COUNT ?? 100);
const MESSAGE_SIZE = Number(process.env.MESSAGE_SIZE ?? 4096);
const ROOM = `demo-${Date.now()}`;
const PEER = fileURLToPath(new URL("./peer.mjs", import.meta.url));

/** Fork one peer process and resolve with its parsed RESULT line. */
function runPeer(role, signalingUrl) {
  return new Promise((resolve, reject) => {
    const child = fork(PEER, {
      stdio: ["ignore", "pipe", "inherit", "ipc"],
      execArgv: ["--experimental-wasm-jspi"],
      env: {
        ...process.env,
        ROLE: role,
        ROOM,
        MESSAGE_COUNT: String(MESSAGE_COUNT),
        MESSAGE_SIZE: String(MESSAGE_SIZE),
        SIGNALING_URL: signalingUrl,
      },
    });

    let out = "";
    child.stdout.on("data", (chunk) => {
      out += chunk;
    });
    child.on("error", reject);
    child.on("exit", (code) => {
      if (code !== 0) {
        reject(new Error(`${role} peer exited with code ${code}`));
        return;
      }
      const line = out.split("\n").find((l) => l.startsWith("RESULT "));
      if (!line) {
        reject(new Error(`${role} peer produced no RESULT`));
        return;
      }
      resolve(JSON.parse(line.slice("RESULT ".length)));
    });
  });
}

async function main() {
  const { server, port } = await startSignalingServer(0);
  const signalingUrl = `http://127.0.0.1:${port}`;
  console.log(`signaling server (HTTP long-poll) on ${signalingUrl}, room '${ROOM}'`);

  const started = performance.now();
  try {
    const [offerer, answerer] = await Promise.all([
      runPeer("offerer", signalingUrl),
      runPeer("answerer", signalingUrl),
    ]);
    const elapsed = performance.now() - started;

    const mibps = offerer.bytesEchoed / (1024 * 1024) / (elapsed / 1000);
    console.log("signaling-demo (Node / @roamhq/wrtc host) result:");
    console.log(`  offerer:  connected=${offerer.connected} sent=${offerer.messagesSent} received=${offerer.messagesReceived}`);
    console.log(`  answerer: connected=${answerer.connected} echoed=${answerer.messagesReceived}`);
    console.log(`  elapsed:  ${elapsed.toFixed(1)} ms  (~${mibps.toFixed(1)} MiB/s round-trip)`);

    const expectedBytes = MESSAGE_COUNT * MESSAGE_SIZE;
    if (!offerer.connected || !answerer.connected) {
      throw new Error("both peers must reach the connected state");
    }
    if (offerer.messagesSent !== MESSAGE_COUNT) {
      throw new Error(`offerer sent ${offerer.messagesSent}, expected ${MESSAGE_COUNT}`);
    }
    if (offerer.messagesReceived !== MESSAGE_COUNT) {
      throw new Error(`offerer received ${offerer.messagesReceived}, expected ${MESSAGE_COUNT}`);
    }
    if (answerer.messagesReceived !== MESSAGE_COUNT) {
      throw new Error(`answerer echoed ${answerer.messagesReceived}, expected ${MESSAGE_COUNT}`);
    }
    if (offerer.bytesEchoed !== expectedBytes) {
      throw new Error(`offerer round-tripped ${offerer.bytesEchoed} bytes, expected ${expectedBytes}`);
    }
    console.log("\nOK: two separate peers connected via HTTP signaling and round-tripped every message.");
  } finally {
    server.close();
  }
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error("signaling-demo failed:", err);
    process.exit(1);
  },
);
