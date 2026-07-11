//! `cli-signaling`: a manual-signaling WebRTC demo driven entirely over
//! `wasi:cli@0.3` stdio.
//!
//! The component drives the host-provided
//! `demo:webrtc-echo/manual-signaling` `peer-connection` with *vanilla*
//! (non-trickle) ICE, so a whole connection needs just two messages: the
//! offerer prints one complete SDP offer (with all ICE candidates already
//! embedded) and the answerer prints one complete SDP answer. Each blob is
//! base64-encoded onto a single line so a user can copy/paste it into the other
//! peer — including the sibling `browser-signaling` component, which speaks the
//! exact same wire format.
//!
//! It is a `wasm32-wasip2` `cdylib` that exports an *async* `wasi:cli/run` via
//! the `wasip3` crate (a synchronous `run` cannot await the async signaling
//! imports). User I/O goes through the `wasi:cli@0.3` stdio streams
//! (`stdin::read-via-stream` / `stdout::write-via-stream`).

use base64::Engine as _;
use wit_bindgen::StreamResult;

wit_bindgen::generate!({
    path: "wit",
    inline: "
        package demo:cli-signaling-bindings;
        world cli-signaling-bindings {
            import demo:webrtc-echo/manual-signaling@0.1.0;
        }
    ",
    generate_all,
});

use demo::webrtc_echo::manual_signaling::PeerConnection;
use lann::webrtc_datachannels::types::{DataChannelOptions, Error};

/// The label used for the negotiated data channel. Both peers observe it.
const CHANNEL_LABEL: &str = "manual-signaling";

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        // Role selection: `offerer` (default) or `answerer`.
        let role = match wasip3::cli::environment::get_arguments().get(1).map(String::as_str) {
            Some("answerer") | Some("answer") => Role::Answerer,
            Some("offerer") | Some("offer") | None => Role::Offerer,
            Some(other) => {
                eprintln!("usage: cli-signaling [offerer|answerer]  (got {other:?})");
                return Err(());
            }
        };

        match drive(role).await {
            Ok(peer_message) => {
                print(&format!(
                    "\nConnected. Message received from peer: {peer_message:?}\n"
                ))
                .await;
                Ok(())
            }
            Err(err) => {
                eprintln!("cli-signaling failed: {err:?}");
                Err(())
            }
        }
    }
}

wasip3::cli::command::export!(Component);

#[derive(Clone, Copy)]
enum Role {
    Offerer,
    Answerer,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Offerer => "offerer",
            Role::Answerer => "answerer",
        }
    }
}

/// Drive the manual-signaling exchange for `role`, returning the message the
/// peer sent over the established data channel.
async fn drive(role: Role) -> Result<String, Error> {
    let pc = PeerConnection::new();
    let mut stdin = Stdin::new();

    let channel = match role {
        Role::Offerer => {
            let options = DataChannelOptions {
                label: CHANNEL_LABEL.to_string(),
                ordered: true,
                max_retransmits: None,
            };
            let offer = pc.create_offer(options).await?;
            present("offer", &offer).await;

            let answer = request(&mut stdin, "answer").await;
            pc.accept_answer(answer).await?;

            print("Applying answer and waiting for the connection to open…\n").await;
            pc.connect().await?
        }
        Role::Answerer => {
            let offer = request(&mut stdin, "offer").await;
            let answer = pc.create_answer(offer).await?;
            present("answer", &answer).await;

            print("Waiting for the connection to open…\n").await;
            pc.connect().await?
        }
    };

    exchange(&channel, role).await
}

/// Send one greeting and receive the peer's greeting over the data channel.
async fn exchange(
    channel: &lann::webrtc_datachannels::data_channels::DataChannel,
    role: Role,
) -> Result<String, Error> {
    // Read the inbound stream first so the peer's message cannot be missed.
    let mut incoming = channel.receive().await;

    let greeting = format!("hello from the {}", role.name());
    let (mut tx, rx) = wit_stream::new::<Vec<u8>>();
    wit_bindgen::spawn(async move {
        let _ = tx.write_all(vec![greeting.into_bytes()]).await;
        drop(tx);
    });

    let send_fut = channel.send(rx);
    let recv_fut = async {
        loop {
            let (status, batch) = incoming.read(Vec::with_capacity(1)).await;
            if let Some(message) = batch.into_iter().next() {
                return String::from_utf8_lossy(&message).into_owned();
            }
            if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
                return String::new();
            }
        }
    };

    let (send_result, peer_message) = futures::join!(send_fut, recv_fut);
    send_result?;
    Ok(peer_message)
}

/// Present an outgoing signaling blob (base64-encoded SDP) on stdout, alone on
/// its own line so the user (or a test harness) can copy it verbatim.
async fn present(title: &str, sdp: &str) {
    let blob = base64::engine::general_purpose::STANDARD.encode(sdp.as_bytes());
    print(&format!(
        "\nCopy this {title} to the other peer (a single line):\n{blob}\n"
    ))
    .await;
}

/// Prompt for and read an incoming signaling blob (base64-encoded SDP) from
/// stdin, returning the decoded SDP text.
async fn request(stdin: &mut Stdin, title: &str) -> String {
    print(&format!(
        "\nPaste the {title} from the other peer, then press Enter:\n"
    ))
    .await;
    loop {
        let Some(line) = stdin.read_line().await else {
            return String::new();
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match base64::engine::general_purpose::STANDARD.decode(line.as_bytes()) {
            Ok(bytes) => return String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => {
                print("That did not look like a base64 blob; try again:\n").await;
            }
        }
    }
}

// --- `wasi:cli@0.3` stdio helpers -----------------------------------------

/// A single stdin reader plus any bytes read past the last newline.
struct Stdin {
    reader: wit_bindgen::StreamReader<u8>,
    buffer: Vec<u8>,
    done: bool,
}

impl Stdin {
    fn new() -> Self {
        let (reader, _future) = wasip3::cli::stdin::read_via_stream();
        Self {
            reader,
            buffer: Vec::new(),
            done: false,
        }
    }

    /// Read a single line (without the trailing newline) from `wasi:cli@0.3`
    /// stdin. Returns `None` once stdin is exhausted with no more data.
    async fn read_line(&mut self) -> Option<String> {
        loop {
            if let Some(pos) = self.buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=pos).collect();
                return Some(
                    String::from_utf8_lossy(&line[..line.len() - 1])
                        .trim_end_matches('\r')
                        .to_string(),
                );
            }
            if self.done {
                if self.buffer.is_empty() {
                    return None;
                }
                let rest = std::mem::take(&mut self.buffer);
                return Some(
                    String::from_utf8_lossy(&rest)
                        .trim_end_matches('\r')
                        .to_string(),
                );
            }
            let (status, chunk) = self.reader.read(Vec::with_capacity(4096)).await;
            self.buffer.extend_from_slice(&chunk);
            if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
                self.done = true;
            }
        }
    }
}

/// Write a string to `wasi:cli@0.3` stdout, awaiting until it is fully flushed.
async fn print(text: &str) {
    let bytes = text.as_bytes().to_vec();
    let (mut tx, rx) = wasip3::wit_stream::new::<u8>();
    let write = wasip3::cli::stdout::write_via_stream(rx);
    let producer = async move {
        let _ = tx.write_all(bytes).await;
        drop(tx);
    };
    let writer = async move {
        let _ = write.await;
    };
    futures::join!(producer, writer);
}
