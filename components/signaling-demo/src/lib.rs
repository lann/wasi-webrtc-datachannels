//! `signaling-demo`: an example component that connects two *separate* WebRTC
//! peers by driving the `wasi:webrtc-data-channels/signaling` peer-connection
//! interface and exchanging SDP/ICE through the demo `rendezvous` mailbox.
//!
//! Unlike `echo-demo` (which uses the host-internal `connect` shortcut), this
//! component performs genuine out-of-band signaling: each peer is its own
//! instance — an `offerer` and an `answerer` — that meet in a shared rendezvous
//! room on an HTTP signaling server, exchange an SDP offer/answer and trickled
//! ICE candidates as opaque blobs, and then form a real peer-to-peer data
//! channel. Once connected, the offerer streams `message-count` messages and
//! reads them back while the answerer echoes each one, proving a full round
//! trip over a WebRTC/SCTP data channel between two independent peers.
//!
//! The guest owns the blob encoding (a tiny line-based text format); the host's
//! `rendezvous` implementation relays those bytes verbatim to and from the
//! server and never inspects them.

wit_bindgen::generate!({
    path: "wit",
    world: "webrtc-signaling-demo",
    generate_all,
});

use exports::demo::webrtc_echo::signaling_demo::{Guest, SignalingConfig, SignalingStats};
use demo::webrtc_echo::rendezvous::{Role, Session};
use wasi::webrtc_data_channels::data_channels::DataChannel;
use wasi::webrtc_data_channels::signaling::{IceCandidate, PeerConnection, SdpType, SessionDescription};
use wasi::webrtc_data_channels::types::{DataChannelOptions, Error};

use futures::future::{self, Either};
use wit_bindgen::StreamResult;

struct Component;

/// One decoded signaling message carried through the rendezvous room.
enum Signal {
    Sdp(SessionDescription),
    Ice(IceCandidate),
}

impl Guest for Component {
    async fn run(config: SignalingConfig) -> Result<SignalingStats, Error> {
        let SignalingConfig {
            room,
            as_role,
            message_count,
            message_size,
        } = config;

        let pc = PeerConnection::new();
        let session = Session::open(room, as_role).await?;

        // Establish the peer connection via signaling, obtaining the channel.
        let channel = match as_role {
            Role::Offerer => connect_offerer(&pc, &session).await?,
            Role::Answerer => connect_answerer(&pc, &session).await?,
        };

        // Connected: run the data round trip over the channel.
        let stats = match as_role {
            Role::Offerer => run_offerer(&channel, message_count, message_size).await,
            Role::Answerer => run_answerer(&channel, message_count).await,
        };

        // Signal to the peer that we are done, releasing the rendezvous room.
        session.close();
        stats
    }
}

/// Offerer signaling: create the channel, publish an offer, then trickle ICE and
/// consume the peer's answer/ICE until the connection is established.
async fn connect_offerer(pc: &PeerConnection, session: &Session) -> Result<DataChannel, Error> {
    let channel = pc.create_data_channel(&DataChannelOptions {
        label: "signaling-demo".to_string(),
        ordered: true,
        max_retransmits: None,
    })?;

    let offer = pc.create_offer().await?;
    let offer_blob = encode_sdp(&offer);
    pc.set_local_description(offer).await?;
    // Send the offer *before* forwarding any local ICE, so the peer observes the
    // description ahead of the candidates that depend on it.
    session.send(offer_blob).await?;

    await_connected(pc, session).await?;
    Ok(channel)
}

/// Answerer signaling: wait for the offer, reply with an answer, then trickle
/// ICE and consume the peer's ICE until connected, adopting the channel the
/// offerer opened.
async fn connect_answerer(pc: &PeerConnection, session: &Session) -> Result<DataChannel, Error> {
    // The first blob from the peer must be the SDP offer.
    let offer = loop {
        match session.recv().await? {
            Some(blob) => match decode(&blob) {
                Some(Signal::Sdp(sdp)) => break sdp,
                // Ignore stray ICE that somehow arrived before the offer.
                Some(Signal::Ice(_)) => continue,
                None => continue,
            },
            None => return Err(Error::Closed),
        }
    };
    pc.set_remote_description(offer).await?;
    let answer = pc.create_answer().await?;
    let answer_blob = encode_sdp(&answer);
    pc.set_local_description(answer).await?;
    // Publish the answer before forwarding local ICE (mirrors the offerer).
    session.send(answer_blob).await?;

    await_connected(pc, session).await?;

    // The offerer opened the channel; adopt the first incoming one.
    let mut incoming = pc.incoming_data_channels();
    let (_, batch) = incoming.read(Vec::with_capacity(1)).await;
    batch
        .into_iter()
        .next()
        .ok_or_else(|| Error::Other("no data channel opened by the peer".to_string()))
}

/// Drive the two signaling loops (forward local ICE, consume the peer's
/// SDP/ICE) concurrently with `wait-connected`, stopping them the moment the
/// connection is established.
async fn await_connected(pc: &PeerConnection, session: &Session) -> Result<(), Error> {
    let signaling = async {
        futures::join!(forward_local_ice(pc, session), consume_remote_signals(pc, session));
    };
    let connected = pc.wait_connected();

    futures::pin_mut!(signaling, connected);
    match future::select(connected, signaling).await {
        // Connected: drop the signaling future, cancelling its pending polls.
        Either::Left((result, _signaling)) => result,
        // The signaling loops never complete on their own before connecting.
        Either::Right((_never, connected)) => connected.await,
    }
}

/// Forward each locally gathered ICE candidate to the peer via rendezvous. Ends
/// when candidate gathering completes (the stream is dropped by the host).
async fn forward_local_ice(pc: &PeerConnection, session: &Session) {
    let mut candidates = pc.local_ice_candidates();
    loop {
        let (status, batch) = candidates.read(Vec::with_capacity(4)).await;
        for candidate in batch {
            let _ = session.send(encode_ice(&candidate)).await;
        }
        if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
            break;
        }
    }
}

/// Apply SDP descriptions and ICE candidates received from the peer. Ends when
/// the peer closes its side of the rendezvous session.
async fn consume_remote_signals(pc: &PeerConnection, session: &Session) {
    loop {
        match session.recv().await {
            Ok(Some(blob)) => match decode(&blob) {
                Some(Signal::Sdp(sdp)) => {
                    let _ = pc.set_remote_description(sdp).await;
                }
                Some(Signal::Ice(candidate)) => {
                    let _ = pc.add_ice_candidate(candidate).await;
                }
                None => {}
            },
            Ok(None) | Err(_) => break,
        }
    }
}

/// Offerer data path: stream `count` messages and read the same number back.
async fn run_offerer(channel: &DataChannel, count: u32, size: u32) -> Result<SignalingStats, Error> {
    let size = size as usize;

    // Detached producer writes each outbound message, one per `list<u8>` element.
    let (mut tx, rx) = wit_stream::new::<Vec<u8>>();
    wit_bindgen::spawn(async move {
        for i in 0..count {
            let remaining = tx.write_all(vec![make_message(size, i)]).await;
            if !remaining.is_empty() {
                break;
            }
        }
        drop(tx);
    });

    let mut incoming = channel.receive().await;
    let send_fut = channel.send(rx);
    let recv_fut = async {
        let mut received: u32 = 0;
        let mut bytes: u64 = 0;
        while received < count {
            let (status, batch) = incoming.read(Vec::with_capacity(count as usize)).await;
            for message in batch {
                received += 1;
                bytes += message.len() as u64;
            }
            if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
                break;
            }
        }
        (received, bytes)
    };

    let (send_result, (received, bytes)) = futures::join!(send_fut, recv_fut);
    send_result?;

    Ok(SignalingStats {
        connected: true,
        messages_sent: count,
        messages_received: received,
        bytes_echoed: bytes,
    })
}

/// Answerer data path: echo every inbound message straight back to the peer.
async fn run_answerer(channel: &DataChannel, count: u32) -> Result<SignalingStats, Error> {
    let mut incoming = channel.receive().await;
    let (mut tx, rx) = wit_stream::new::<Vec<u8>>();

    let send_fut = channel.send(rx);
    let echo_fut = async {
        let mut echoed: u32 = 0;
        let mut bytes: u64 = 0;
        while echoed < count {
            let (status, batch) = incoming.read(Vec::with_capacity(count as usize)).await;
            for message in batch {
                echoed += 1;
                bytes += message.len() as u64;
                // Echo the message verbatim, preserving its boundary.
                let remaining = tx.write_all(vec![message]).await;
                if !remaining.is_empty() {
                    break;
                }
            }
            if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
                break;
            }
        }
        drop(tx);
        (echoed, bytes)
    };

    let (send_result, (echoed, bytes)) = futures::join!(send_fut, echo_fut);
    send_result?;

    Ok(SignalingStats {
        connected: true,
        messages_sent: echoed,
        messages_received: echoed,
        bytes_echoed: bytes,
    })
}

/// Encode an SDP description as `sdp\n<kind>\n<sdp-body>`.
fn encode_sdp(description: &SessionDescription) -> Vec<u8> {
    let kind = match description.kind {
        SdpType::Offer => "offer",
        SdpType::Answer => "answer",
        SdpType::Pranswer => "pranswer",
        SdpType::Rollback => "rollback",
    };
    format!("sdp\n{kind}\n{}", description.sdp).into_bytes()
}

/// Encode an ICE candidate as `ice\n<candidate>\n<sdp-mid>\n<sdp-mline-index>`,
/// with empty fields standing in for `none`.
fn encode_ice(candidate: &IceCandidate) -> Vec<u8> {
    let mid = candidate.sdp_mid.clone().unwrap_or_default();
    let mline = candidate
        .sdp_mline_index
        .map(|i| i.to_string())
        .unwrap_or_default();
    format!("ice\n{}\n{}\n{}", candidate.candidate, mid, mline).into_bytes()
}

/// Decode a blob produced by `encode_sdp` / `encode_ice`.
fn decode(blob: &[u8]) -> Option<Signal> {
    let text = std::str::from_utf8(blob).ok()?;
    if let Some(rest) = text.strip_prefix("sdp\n") {
        let (kind, sdp) = rest.split_once('\n')?;
        let kind = match kind {
            "offer" => SdpType::Offer,
            "answer" => SdpType::Answer,
            "pranswer" => SdpType::Pranswer,
            "rollback" => SdpType::Rollback,
            _ => return None,
        };
        Some(Signal::Sdp(SessionDescription {
            kind,
            sdp: sdp.to_string(),
        }))
    } else if let Some(rest) = text.strip_prefix("ice\n") {
        let mut parts = rest.splitn(3, '\n');
        let candidate = parts.next()?.to_string();
        let mid = parts.next().unwrap_or("");
        let mline = parts.next().unwrap_or("");
        Some(Signal::Ice(IceCandidate {
            candidate,
            sdp_mid: (!mid.is_empty()).then(|| mid.to_string()),
            sdp_mline_index: (!mline.is_empty()).then(|| mline.parse().ok()).flatten(),
        }))
    } else {
        None
    }
}

/// Build a deterministic `size`-byte message tagged with its index.
fn make_message(size: usize, index: u32) -> Vec<u8> {
    let mut message = vec![0u8; size];
    let tag = index.to_le_bytes();
    for (slot, byte) in message.iter_mut().zip(tag.iter().cycle()) {
        *slot = *byte;
    }
    message
}

export!(Component);
