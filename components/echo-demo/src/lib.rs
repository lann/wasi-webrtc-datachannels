//! `echo-demo`: an example WebAssembly component that exercises a WebRTC data
//! channel entirely through component-model `stream`s.
//!
//! The component is host-agnostic. It imports the `connect` interface to obtain
//! a data channel wired to a host-provided echo endpoint, then:
//!
//!   1. spawns a producer that writes `message-count` messages into an outbound
//!      `stream<list<u8>>`,
//!   2. hands that stream to `data-channel.send` (the async streaming import),
//!   3. concurrently reads the inbound `stream<list<u8>>` returned by
//!      `data-channel.receive`, counting the echoed messages/bytes,
//!
//! all within a single cooperative async task (steps 2 and 3 run under
//! `futures::join!`). The same component binary runs unchanged under the Node
//! (`jco` + `@roamhq/wrtc`) host and the Wasmtime (`webrtc-rs`) host, which is
//! what demonstrates cross-implementation compatibility.

wit_bindgen::generate!({
    path: "wit",
    world: "webrtc-echo-demo",
    generate_all,
});

use exports::wasi::webrtc_data_channels::demo::{DemoConfig, DemoStats, Guest};
use wasi::webrtc_data_channels::connect;
use wasi::webrtc_data_channels::types::{DataChannelOptions, Error};
use wit_bindgen::StreamResult;

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

        // Outbound message pipeline: a detached producer writes each message
        // into `tx`; `send` drains `rx` into the transport.
        let (mut tx, rx) = wit_stream::new::<Vec<u8>>();
        wit_bindgen::spawn(async move {
            for i in 0..count {
                let message = make_message(size, i);
                // `stream<list<u8>>` elements are whole messages; write one at a
                // time so boundaries are preserved end to end.
                let remaining = tx.write_all(vec![message]).await;
                if !remaining.is_empty() {
                    // The reader was dropped (channel closed); stop producing.
                    break;
                }
            }
            drop(tx);
        });

        // Inbound message stream. Created before `send` starts so no echoed
        // message can be missed.
        let mut incoming = channel.receive();

        // Drive send and receive concurrently on this single task.
        let send_fut = channel.send(rx);
        let recv_fut = async {
            let mut messages_received: u32 = 0;
            let mut bytes_echoed: u64 = 0;
            while messages_received < count {
                let (status, batch) = incoming.read(Vec::with_capacity(count as usize)).await;
                for message in batch {
                    messages_received += 1;
                    bytes_echoed += message.len() as u64;
                }
                if matches!(status, StreamResult::Dropped | StreamResult::Cancelled) {
                    break;
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
