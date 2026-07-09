// One peer of the signaling demo. Reads its role and run parameters from the
// environment, instantiates the transpiled `signaling-demo` component, and
// drives the exported async `run`. Because each peer is a separate Node process
// (see `run.mjs`), the offerer and answerer are genuinely independent WebRTC
// peers that meet only through the shared rendezvous room.
//
// Env:
//   ROLE           'offerer' | 'answerer'
//   ROOM           shared rendezvous room name
//   MESSAGE_COUNT  messages the offerer sends (default 100)
//   MESSAGE_SIZE   bytes per message (default 4096)
//   SIGNALING_URL  signaling server base URL (read by rendezvous.js)
import { signalingDemo } from "../generated-signaling/signaling-demo.js";

const role = process.env.ROLE;
const room = process.env.ROOM ?? "demo";
const messageCount = Number(process.env.MESSAGE_COUNT ?? 100);
const messageSize = Number(process.env.MESSAGE_SIZE ?? 4096);

if (role !== "offerer" && role !== "answerer") {
  console.error(`peer: invalid ROLE '${role}' (expected 'offerer' or 'answerer')`);
  process.exit(2);
}

async function main() {
  const stats = await signalingDemo.run({ room, asRole: role, messageCount, messageSize });
  // Emit machine-readable stats for the driver to aggregate.
  process.stdout.write(
    `RESULT ${JSON.stringify({
      role,
      connected: stats.connected,
      messagesSent: stats.messagesSent,
      messagesReceived: stats.messagesReceived,
      bytesEchoed: Number(stats.bytesEchoed),
    })}\n`,
  );
}

main().then(
  () => process.exit(0),
  (err) => {
    console.error(`peer (${role}) failed:`, err);
    process.exit(1);
  },
);
