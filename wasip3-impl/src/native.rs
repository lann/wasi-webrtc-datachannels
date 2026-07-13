//! A native reference driver: [`NativePeer`].
//!
//! This backs the sans-I/O [`SansIoPeer`] with a Tokio [`UdpSocket`] and timer,
//! running the event loop on a spawned task. It is the *host-side* driver — the
//! choice made for the first milestone, because it is what the workspace can
//! build and test in CI. The [`SansIoPeer`] core it drives does no I/O of its
//! own, so a future guest can drive the same core over `wasi:sockets` without
//! changing it.
//!
//! Only the answerer role is wired here (the demo `webrtc-rs` hosts are the
//! offerers); [`SansIoPeer`] already exposes the offer primitives an offerer
//! driver would need.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::peer::{PeerEvent, SansIoPeer};

/// The safety-net wake interval, so the loop re-checks timers even if the stack
/// reports no deadline. A short fixed cap bounds how long the loop can sleep
/// when [`SansIoPeer::poll_timeout`] returns `None`, ensuring retransmit and
/// keep-alive timers still fire promptly; 50ms trades negligible idle wakeups
/// for low latency.
const MAX_WAIT: Duration = Duration::from_millis(50);

/// An inbound data-channel message surfaced by [`NativePeer`].
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Whether the payload was sent as text (UTF-8) rather than binary.
    pub text: bool,
    /// The message payload.
    pub data: Vec<u8>,
}

/// A command sent from a [`NativePeer`] handle into its driver task.
enum Command {
    Send { text: bool, data: Vec<u8> },
}

/// The result of setting up an answerer: a driven [`NativePeer`] plus the
/// signaling blobs to hand back to the remote offerer.
pub struct Answered {
    /// The handle to the running peer.
    pub peer: NativePeer,
    /// The SDP answer to return to the offerer.
    pub answer_sdp: String,
    /// The local host candidate to trickle to the offerer.
    pub local_candidate: String,
}

/// A handle to a sans-I/O peer whose event loop runs on a spawned Tokio task.
pub struct NativePeer {
    cmd_tx: mpsc::UnboundedSender<Command>,
    msg_rx: mpsc::UnboundedReceiver<InboundMessage>,
    connected: Option<oneshot::Receiver<()>>,
}

impl NativePeer {
    /// Set up an answerer over `socket`: apply `remote_offer_sdp`, gather a host
    /// candidate for the socket, produce an answer, and spawn the event loop.
    pub async fn answer(socket: UdpSocket, remote_offer_sdp: String) -> Result<Answered> {
        let local_addr = socket.local_addr()?;

        let mut peer = SansIoPeer::answerer()?;
        peer.set_remote_offer(remote_offer_sdp)?;
        let local_candidate = peer.add_local_host_candidate(local_addr)?;
        let answer_sdp = peer.create_answer()?;

        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        let (connected_tx, connected_rx) = oneshot::channel();

        tokio::spawn(drive(peer, socket, cmd_rx, msg_tx, connected_tx));

        Ok(Answered {
            peer: NativePeer {
                cmd_tx,
                msg_rx,
                connected: Some(connected_rx),
            },
            answer_sdp,
            local_candidate,
        })
    }

    /// Resolve once the connection reaches the `connected` state.
    pub async fn wait_connected(&mut self) -> Result<()> {
        match self.connected.take() {
            Some(rx) => rx
                .await
                .map_err(|_| anyhow!("peer closed before connecting")),
            None => Err(anyhow!("wait_connected already awaited")),
        }
    }

    /// Receive the next inbound message, or `None` once the loop has stopped.
    pub async fn next_message(&mut self) -> Option<InboundMessage> {
        self.msg_rx.recv().await
    }

    /// Queue a text (UTF-8) message for the peer's first data channel. Buffered
    /// until that channel opens.
    pub fn send_text(&self, text: &str) -> Result<()> {
        self.cmd_tx
            .send(Command::Send {
                text: true,
                data: text.as_bytes().to_vec(),
            })
            .map_err(|_| anyhow!("driver task has stopped"))
    }

    /// Queue a binary message for the peer's first data channel. Buffered until
    /// that channel opens.
    pub fn send_binary(&self, data: &[u8]) -> Result<()> {
        self.cmd_tx
            .send(Command::Send {
                text: false,
                data: data.to_vec(),
            })
            .map_err(|_| anyhow!("driver task has stopped"))
    }
}

/// The sans-I/O event loop: flush outbound datagrams, drain events, and wait on
/// the earliest of a timer, a command, or an inbound datagram.
async fn drive(
    mut peer: SansIoPeer,
    socket: UdpSocket,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    msg_tx: mpsc::UnboundedSender<InboundMessage>,
    connected_tx: oneshot::Sender<()>,
) {
    let mut primary = None;
    let mut pending: Vec<(bool, Vec<u8>)> = Vec::new();
    let mut connected_tx = Some(connected_tx);
    let mut buf = vec![0u8; 2048];

    loop {
        flush_transmits(&mut peer, &socket).await;

        for event in peer.drain_events() {
            match event {
                PeerEvent::Connected => {
                    if let Some(tx) = connected_tx.take() {
                        let _ = tx.send(());
                    }
                }
                PeerEvent::ChannelOpen { id, .. } => {
                    if primary.is_none() {
                        primary = Some(id);
                        for (text, data) in pending.drain(..) {
                            send_now(&mut peer, id, text, &data);
                        }
                    }
                }
                PeerEvent::Message { text, data, .. } => {
                    if msg_tx.send(InboundMessage { text, data }).is_err() {
                        return;
                    }
                }
                PeerEvent::Failed | PeerEvent::Closed => return,
            }
        }

        // Sends triggered by draining events (e.g. flushing `pending`) queue
        // outbound datagrams; get them onto the wire before we park.
        flush_transmits(&mut peer, &socket).await;

        let now = Instant::now();
        let deadline = peer.poll_timeout().unwrap_or(now + MAX_WAIT);
        let delay = deadline.saturating_duration_since(now).min(MAX_WAIT);

        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);

        tokio::select! {
            _ = &mut timer => {
                peer.handle_timeout(Instant::now());
            }
            command = cmd_rx.recv() => match command {
                Some(Command::Send { text, data }) => match primary {
                    Some(id) => send_now(&mut peer, id, text, &data),
                    None => pending.push((text, data)),
                },
                // Handle dropped: the caller is gone, so shut the peer down.
                None => {
                    peer.close();
                    flush_transmits(&mut peer, &socket).await;
                    return;
                }
            },
            received = socket.recv_from(&mut buf) => {
                if let Ok((n, from)) = received {
                    let local = socket.local_addr().expect("bound socket must have a local address (internal invariant)");
                    peer.handle_input(&buf[..n], from, local, Instant::now());
                }
            }
        }
    }
}

/// Send every currently queued outbound datagram.
async fn flush_transmits(peer: &mut SansIoPeer, socket: &UdpSocket) {
    while let Some(transmit) = peer.poll_transmit() {
        let _ = socket
            .send_to(&transmit.payload, transmit.destination)
            .await;
    }
}

/// Hand one message to a specific channel, ignoring send errors (a closed
/// channel surfaces as a `Closed` event instead).
fn send_now(
    peer: &mut SansIoPeer,
    id: rtc::data_channel::RTCDataChannelId,
    text: bool,
    data: &[u8],
) {
    let _ = if text {
        peer.send_text(id, &String::from_utf8_lossy(data))
    } else {
        peer.send_binary(id, data)
    };
}
