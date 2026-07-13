//! `wasip3-cli`: a self-contained WebRTC data-channel demo that runs the
//! **entire** WebRTC stack *inside* a wasm component.
//!
//! Unlike the `cli-signaling` component — which drives a host-provided
//! `manual-signaling` `peer-connection` (the `webrtc-rs` engine runs host-side)
//! — this component runs the sans-I/O `rtc` stack in-guest via
//! [`wasip3_webrtc_datachannels::GuestPeer`], driving it over `wasi:sockets`
//! UDP and `wasi:clocks` timers. It stands up an offerer and an answerer,
//! connects them over UDP loopback, and exchanges one message each way,
//! proving the sans-I/O core interoperates with itself entirely within WASIp3.
//!
//! It is a `wasm32-wasip2` `cdylib` that exports an *async* `wasi:cli/run` via
//! the `wasip3` crate (a synchronous `run` cannot await the async
//! `wasi:sockets` imports the driver depends on). Progress is written to
//! `wasi:cli@0.3` stdout.
//!
//! Run it with `wasmtime run` once the async component-model and WASIp3
//! (including `wasi:sockets` UDP) host capabilities are provisioned:
//!
//! ```sh
//! wasmtime run -W component-model-async=y -S cli -S p3 -S inherit-network \
//!     wasip3-cli.wasm
//! ```

use std::cell::Cell;
use std::net::{IpAddr, Ipv4Addr};
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use wasip3_webrtc_datachannels::{GuestPeer, PeerEvent, SansIoPeer};

/// The label of the negotiated data channel. Both peers observe it.
const CHANNEL_LABEL: &str = "wasip3-demo";

/// Loopback: the two in-process peers reach each other over `127.0.0.1`.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// A safety cap so a connection that never establishes fails instead of hanging.
const OVERALL_TIMEOUT: Duration = Duration::from_secs(30);

struct Component;

impl wasip3::exports::cli::run::Guest for Component {
    async fn run() -> Result<(), ()> {
        match demo().await {
            Ok(Report {
                offerer_received,
                answerer_received,
            }) => {
                print(&format!(
                    "\nConnected over UDP loopback inside the component.\n\
                     Offerer received: {offerer_received:?}\n\
                     Answerer received: {answerer_received:?}\n"
                ))
                .await;
                Ok(())
            }
            Err(err) => {
                print(&format!("wasip3-cli failed: {err:?}\n")).await;
                Err(())
            }
        }
    }
}

wasip3::cli::command::export!(Component);

/// What each peer received over the data channel once the round trip completed.
struct Report {
    offerer_received: String,
    answerer_received: String,
}

/// Stand up an offerer and an answerer, exchange their SDP/candidates directly
/// (no external signaling), then drive both over UDP loopback until each has
/// received the other's greeting.
async fn demo() -> Result<Report> {
    print("Setting up the offerer and answerer inside the component…\n").await;

    // --- offerer -----------------------------------------------------------
    let (offerer, offer_sdp) = SansIoPeer::offerer(CHANNEL_LABEL, true, None)?;
    let mut offerer = GuestPeer::bind(offerer, LOOPBACK)?;
    let offerer_addr = offerer.local_addr();
    let offerer_candidate = offerer.peer().add_local_host_candidate(offerer_addr)?;

    // --- answerer ----------------------------------------------------------
    let mut answerer = SansIoPeer::answerer()?;
    answerer.set_remote_offer(offer_sdp)?;
    let mut answerer = GuestPeer::bind(answerer, LOOPBACK)?;
    let answerer_addr = answerer.local_addr();
    let answerer_candidate = answerer.peer().add_local_host_candidate(answerer_addr)?;
    let answer_sdp = answerer.peer().create_answer()?;

    // --- exchange the answer and the host candidates -----------------------
    offerer.peer().set_remote_answer(answer_sdp)?;
    offerer.peer().add_remote_candidate(answerer_candidate)?;
    answerer.peer().add_remote_candidate(offerer_candidate)?;

    print(&format!(
        "Offerer bound to {offerer_addr}, answerer bound to {answerer_addr}. \
         Connecting…\n"
    ))
    .await;

    // Drive both peers concurrently on the single cooperative task. `received`
    // keeps each loop pumping until *both* peers have the other's message, so
    // neither stops before it has delivered (and had acknowledged) its own.
    let received = Rc::new(Cell::new(0u32));
    let offerer_run = drive(&mut offerer, "offerer", received.clone());
    let answerer_run = drive(&mut answerer, "answerer", received.clone());
    let (offerer_received, answerer_received) = futures::join!(offerer_run, answerer_run);

    Ok(Report {
        offerer_received: offerer_received?,
        answerer_received: answerer_received?,
    })
}

/// Pump a single peer: flush outbound datagrams, react to events (send the
/// greeting once the channel opens, record the peer's message), and wait on the
/// earliest of a timer or an inbound datagram. Returns the message received
/// from the peer.
async fn drive(peer: &mut GuestPeer, role: &str, received: Rc<Cell<u32>>) -> Result<String> {
    let greeting = format!("hello from the {role}");
    let deadline = Instant::now() + OVERALL_TIMEOUT;

    let mut channel = None;
    let mut got: Option<String> = None;

    loop {
        peer.flush().await;

        for event in peer.drain_events() {
            match event {
                PeerEvent::ChannelOpen { id, .. } => {
                    if channel.is_none() {
                        channel = Some(id);
                        peer.peer().send_binary(id, greeting.as_bytes())?;
                    }
                }
                PeerEvent::Message { data, text, .. } => {
                    if got.is_none() {
                        got = Some(decode(&data, text));
                        received.set(received.get() + 1);
                    }
                }
                PeerEvent::Connected => {}
                PeerEvent::Failed => return Err(anyhow!("{role} connection failed")),
                PeerEvent::Closed => {
                    return got.ok_or_else(|| anyhow!("{role} closed before exchanging a message"));
                }
            }
        }

        // Flush any datagrams the drained events queued (the greeting and the
        // acknowledgement of a received message) before parking.
        peer.flush().await;

        // Done once this peer has its message and the other peer has too, so
        // neither loop stops while the other still needs an acknowledgement.
        if got.is_some() && received.get() >= 2 {
            peer.peer().close();
            peer.flush().await;
            return got.ok_or_else(|| anyhow!("{role} lost its received message"));
        }

        if Instant::now() >= deadline {
            return Err(anyhow!("{role} timed out before completing the round trip"));
        }

        peer.wait().await;
    }
}

/// Decode a data-channel payload into a `String` for reporting.
fn decode(data: &[u8], _text: bool) -> String {
    String::from_utf8_lossy(data).into_owned()
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
