//! Test guest: exercises the reusable `manual-signaling` + `data-channels`
//! interfaces implemented by `wasmtime-wasi-webrtc-datachannels`.
//!
//! It stands up an offerer and an answerer `peer-connection` entirely in-guest,
//! performs the vanilla (non-trickle) offer/answer exchange the host satisfies,
//! opens the negotiated data channel, then streams `count` messages of `size`
//! bytes from the offerer and reads them all back on the answerer. This drives
//! every method the crate implements for these two interfaces.

wit_bindgen::generate!({
    path: "wit",
    world: "manual-signaling-test",
    generate_all,
});

use exports::test::webrtc_manual_signaling::runner::{Guest, Report};
use wasi::webrtc_data_channels::manual_signaling::PeerConnection;
use wasi::webrtc_data_channels::types::{DataChannelOptions, Error};
use wit_bindgen::StreamResult;

const CHANNEL_LABEL: &str = "manual-signaling-test";

struct Component;

impl Guest for Component {
    async fn run(count: u32, size: u32) -> Result<Report, Error> {
        let offerer = PeerConnection::new();
        let answerer = PeerConnection::new();

        // Vanilla offer/answer exchange, offerer -> answerer -> offerer.
        let options = DataChannelOptions {
            label: CHANNEL_LABEL.to_string(),
            ordered: true,
            max_retransmits: None,
        };
        let offer = offerer.create_offer(options).await?;
        let answer = answerer.create_answer(offer).await?;
        offerer.accept_answer(answer).await?;

        // Both sides block until the channel opens; drive them concurrently.
        let (offerer_channel, answerer_channel) =
            futures::join!(offerer.connect(), answerer.connect());
        let offerer_channel = offerer_channel?;
        let answerer_channel = answerer_channel?;

        let label = offerer_channel.label();

        // Read the inbound stream on the answerer first so no message is missed.
        let mut incoming = answerer_channel.receive().await;

        // Outbound pipeline on the offerer: a detached producer writes each
        // message into `tx`; `send` drains `rx` into the transport.
        let (mut tx, rx) = wit_stream::new::<Vec<u8>>();
        wit_bindgen::spawn(async move {
            for i in 0..count {
                let message = make_message(size as usize, i);
                let remaining = tx.write_all(vec![message]).await;
                if !remaining.is_empty() {
                    break;
                }
            }
            drop(tx);
        });

        let send_fut = offerer_channel.send(rx);
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

        Ok(Report {
            label,
            sent: count,
            received,
            bytes,
        })
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
