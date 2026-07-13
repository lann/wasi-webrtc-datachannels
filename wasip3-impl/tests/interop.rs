//! End-to-end interop: a `webrtc-rs` offerer against this crate's sans-I/O
//! answerer, round-tripping messages over a real DTLS + SCTP data channel.
//!
//! This mirrors the demo hosts' manual-signaling exchange (complete offer/answer
//! SDP blobs, loopback ICE) but replaces the answering side with the sans-I/O
//! [`NativePeer`]. It proves the `rtc` `wasi` fork's transport interoperates
//! with the `webrtc-rs` stack the rest of the repo uses.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use wasip3_webrtc_datachannels::NativePeer;

use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;

/// Number of messages the offerer sends (and expects echoed back).
const COUNT: usize = 16;

#[tokio::test(flavor = "multi_thread")]
async fn wasip3_answerer_round_trips_with_webrtc_rs() {
    let report = tokio::time::timeout(Duration::from_secs(60), round_trip())
        .await
        .expect("interop round trip timed out")
        .expect("interop round trip failed");

    assert_eq!(report.echoed, COUNT, "answerer should echo every message");
    assert_eq!(
        report.received_back, COUNT,
        "offerer should receive every echo back over the data channel"
    );
}

struct Report {
    /// Messages the sans-I/O answerer received and echoed.
    echoed: usize,
    /// Echoes the `webrtc-rs` offerer received back.
    received_back: usize,
}

async fn round_trip() -> Result<Report> {
    // --- webrtc-rs offerer -------------------------------------------------
    let mut setting = SettingEngine::default();
    // Two same-host peers only reach each other over loopback.
    setting.set_include_loopback_candidate(true);
    let api = APIBuilder::new().with_setting_engine(setting).build();
    let offerer = Arc::new(api.new_peer_connection(RTCConfiguration::default()).await?);

    let channel = offerer.create_data_channel("interop", None).await?;

    // On open, send COUNT text messages.
    let sender = channel.clone();
    channel.on_open(Box::new(move || {
        let sender = sender.clone();
        Box::pin(async move {
            for i in 0..COUNT {
                let _ = sender.send_text(format!("msg-{i}")).await;
            }
        })
    }));

    // Count echoes received back; report COUNT once they have all arrived.
    let (done_tx, mut done_rx) = mpsc::channel::<usize>(1);
    let received_back = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = received_back.clone();
    channel.on_message(Box::new(move |_: DataChannelMessage| {
        let counter = counter.clone();
        let done_tx = done_tx.clone();
        Box::pin(async move {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if n == COUNT {
                let _ = done_tx.try_send(n);
            }
        })
    }));

    let offer = offerer.create_offer(None).await?;
    let mut gather = offerer.gathering_complete_promise().await;
    offerer.set_local_description(offer).await?;
    let _ = gather.recv().await;
    let offer_sdp = offerer
        .local_description()
        .await
        .ok_or_else(|| anyhow!("offerer produced no local description"))?
        .sdp;

    // --- sans-I/O answerer -------------------------------------------------
    let socket = UdpSocket::bind("127.0.0.1:0").await?;
    let answered = NativePeer::answer(socket, offer_sdp).await?;

    // Complete signaling on the offerer with the answer + trickled candidate.
    offerer
        .set_remote_description(RTCSessionDescription::answer(answered.answer_sdp)?)
        .await?;
    offerer
        .add_ice_candidate(RTCIceCandidateInit {
            candidate: answered.local_candidate,
            sdp_mid: Some("0".to_string()),
            sdp_mline_index: Some(0),
            username_fragment: None,
        })
        .await?;

    // Echo every inbound message back until COUNT have round-tripped.
    let mut peer = answered.peer;
    let echo = tokio::spawn(async move {
        let mut echoed = 0usize;
        while echoed < COUNT {
            match peer.next_message().await {
                Some(message) => {
                    if message.text {
                        peer.send_text(&String::from_utf8_lossy(&message.data))
                            .map_err(|e| anyhow!("echo send failed: {e}"))?;
                    } else {
                        peer.send_binary(&message.data)
                            .map_err(|e| anyhow!("echo send failed: {e}"))?;
                    }
                    echoed += 1;
                }
                None => break,
            }
        }
        Ok::<usize, anyhow::Error>(echoed)
    });

    let received_back = done_rx
        .recv()
        .await
        .ok_or_else(|| anyhow!("offerer never received all echoes"))?;
    let echoed = echo.await??;

    Ok(Report {
        echoed,
        received_back,
    })
}
