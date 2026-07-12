//! Test guest: exercises the demo-only `manual-signaling` interface together
//! with the `data-channels` interface implemented by
//! `wasmtime-webrtc-datachannels`.
//!
//! It stands up an offerer and an answerer `peer-connection` entirely in-guest,
//! performs the vanilla (non-trickle) offer/answer exchange the host satisfies,
//! opens the negotiated data channel, then sends `count` messages of `size`
//! bytes from the offerer and reads them all back on the answerer. This drives
//! every method the manual-signaling host and the crate's `data-channels` host
//! implement for these two interfaces.

wit_bindgen::generate!({
    path: "wit",
    world: "manual-signaling-test",
    generate_all,
});

use demo::webrtc_echo::manual_signaling::PeerConnection;
use exports::test::webrtc_manual_signaling::runner::{Guest, Report};
use lann::webrtc_datachannels::data_channels::{Message, MessageKind, StreamMessage};
use lann::webrtc_datachannels::types::{DataChannelOptions, Error};
use wit_bindgen::spawn;

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

        // Send on the offerer and receive on the answerer concurrently. Each
        // call carries exactly one message, preserving message boundaries.
        let send_fut = async {
            for i in 0..count {
                offerer_channel
                    .send(Message::Binary(make_message(size as usize, i)))
                    .await?;
            }
            Ok::<(), Error>(())
        };
        let recv_fut = async {
            let mut received: u32 = 0;
            let mut bytes: u64 = 0;
            while received < count {
                match answerer_channel.receive().await {
                    Ok(message) => {
                        received += 1;
                        bytes += message_len(&message) as u64;
                    }
                    Err(_) => break,
                }
            }
            (received, bytes)
        };

        let (send_result, (received, bytes)) = futures::join!(send_fut, recv_fut);
        send_result?;

        // Second pass: send another `count` messages through `send-via-stream`,
        // closing the stream's write end while messages are still queued, and
        // read them back with `receive`. This exercises the streaming send path
        // and its finish semantics (every queued message must still be sent).
        let stream_send_fut = async {
            let (mut messages_tx, messages_rx) = wit_stream::new::<StreamMessage>();
            // Produce every message on a spawned task, feeding each message's
            // payload through its own byte stream, then drop `messages_tx` to
            // close the write end while the host may still be draining messages.
            spawn(async move {
                for i in 0..count {
                    let payload = make_message(size as usize, i);
                    let (mut data_tx, data_rx) = wit_stream::new::<u8>();
                    spawn(async move {
                        let _ = data_tx.write_all(payload).await;
                    });
                    let _ = messages_tx
                        .write_one(StreamMessage {
                            kind: MessageKind::Binary,
                            length: size,
                            data: data_rx,
                        })
                        .await;
                }
            });
            offerer_channel.send_via_stream(messages_rx).await
        };
        let stream_recv_fut = async {
            let mut received: u32 = 0;
            while received < count {
                match answerer_channel.receive().await {
                    Ok(_) => received += 1,
                    Err(_) => break,
                }
            }
            received
        };
        let (stream_send_result, stream_received) =
            futures::join!(stream_send_fut, stream_recv_fut);
        let stream_sent = match stream_send_result {
            Ok(()) => count,
            Err(err) => err.sent as u32,
        };

        Ok(Report {
            label,
            sent: count,
            received,
            bytes,
            stream_sent,
            stream_received,
        })
    }
}

/// The byte length of a received message, regardless of kind.
fn message_len(message: &Message) -> usize {
    match message {
        Message::Binary(bytes) => bytes.len(),
        Message::String(text) => text.len(),
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
