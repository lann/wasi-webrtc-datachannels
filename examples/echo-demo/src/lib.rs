//! `echo-demo`: an example WebAssembly component that exercises a WebRTC data
//! channel one message at a time.
//!
//! The component is host-agnostic. It imports the `connect` interface to obtain
//! a data channel wired to a host-provided echo endpoint, then:
//!
//!   1. spawns a producer loop that sends `message-count` messages through
//!      `data-channel.send` (the async per-message import),
//!   2. concurrently reads the echoed messages back through
//!      `data-channel.receive`, counting the messages/bytes,
//!
//! all within a single cooperative async task (the send and receive loops run
//! under `futures::join!`). The same component binary runs unchanged under the
//! Node (`jco` + `@roamhq/wrtc`) host and the Wasmtime (`webrtc-rs`) host, which
//! is what demonstrates cross-implementation compatibility.

wit_bindgen::generate!({
    path: "wit",
    world: "webrtc-echo-demo",
    generate_all,
});

use demo::webrtc_echo::connect;
use exports::demo::webrtc_echo::demo::{DemoConfig, DemoStats, Guest};
use lann::webrtc_datachannels::data_channels::Message;
use lann::webrtc_datachannels::types::{DataChannelOptions, Error};

struct Component;

impl Guest for Component {
    async fn run(config: DemoConfig) -> Result<DemoStats, Error> {
        let count = config.message_count;
        let size = config.message_size as usize;

        // Ask the host for a channel connected to its echo endpoint.
        let channel = connect::open_echo(DataChannelOptions {
            label: "echo".to_string(),
            ordered: true,
            max_retransmits: None,
        })
        .await?;

        // Drive send and receive concurrently on this single task. Each call
        // carries exactly one message, preserving WebRTC message boundaries.
        let send_fut = async {
            for i in 0..count {
                channel.send(Message::Binary(make_message(size, i))).await?;
            }
            Ok::<(), Error>(())
        };
        let recv_fut = async {
            let mut messages_received: u32 = 0;
            let mut bytes_echoed: u64 = 0;
            while messages_received < count {
                match channel.receive().await {
                    Ok(message) => {
                        messages_received += 1;
                        bytes_echoed += message_len(&message) as u64;
                    }
                    // The channel closed before every message was echoed back.
                    Err(_) => break,
                }
            }
            (messages_received, bytes_echoed)
        };

        let (send_result, (messages_received, bytes_echoed)) = futures::join!(send_fut, recv_fut);
        send_result?;

        Ok(DemoStats {
            messages_sent: count,
            messages_received,
            bytes_echoed,
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

/// Build a deterministic `size`-byte message tagged with its index so a peer
/// (or a stricter demo) could verify ordering.
fn make_message(size: usize, index: u32) -> Vec<u8> {
    let mut message = vec![0u8; size];
    let tag = index.to_le_bytes();
    for (slot, byte) in message.iter_mut().zip(tag.iter().cycle()) {
        *slot = *byte;
    }
    message
}

export!(Component);
