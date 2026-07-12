//! The `webrtc-rs`-backed [`DataChannel`] host resource and helpers.
//!
//! [`DataChannel`] is the concrete host type mapped onto the
//! `lann:webrtc-datachannels/connections.data-channel` resource. It wraps an
//! open `webrtc-rs` [`RTCDataChannel`], its inbound-message stream, and the peer
//! connection(s) that must outlive it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use bytes::Bytes;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::oneshot;
use futures::future::Shared;
use futures::lock::Mutex as AsyncMutex;
use futures::FutureExt;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;

/// A single inbound data-channel message together with its kind.
///
/// WebRTC distinguishes binary from text (UTF-8) messages; the host preserves
/// that distinction so `receive` can surface the correct `message` variant.
#[derive(Clone, Debug)]
pub struct InboundMessage {
    /// Whether the message was sent as text (UTF-8) rather than binary.
    pub is_string: bool,
    /// The raw message payload.
    pub data: Vec<u8>,
}

impl InboundMessage {
    /// A binary inbound message.
    pub fn binary(data: Vec<u8>) -> Self {
        Self {
            is_string: false,
            data,
        }
    }

    /// A text (UTF-8) inbound message.
    pub fn text(data: Vec<u8>) -> Self {
        Self {
            is_string: true,
            data,
        }
    }
}

/// Host state behind a `data-channel` resource.
///
/// A connected, bidirectional WebRTC data channel plus its inbound-message
/// stream. The inbound receiver is shared behind an async mutex so concurrent
/// `receive` calls are each handed the next message in turn.
pub struct DataChannel {
    channel: Arc<RTCDataChannel>,
    /// Inbound messages, delivered one per `receive` call. Behind an async mutex
    /// so concurrent receivers serialize and each takes the next message.
    incoming: Arc<AsyncMutex<UnboundedReceiver<InboundMessage>>>,
    /// Set once `receive-via-stream` has claimed the inbound messages. While set,
    /// `receive` and `receive-via-stream` both fail with `receiving-via-stream`.
    stream_receiving: Arc<AtomicBool>,
    /// Sender fired when `receive-via-stream` is first called. Held in a mutex so
    /// the first caller takes it (claiming the channel) and all later callers
    /// observe `None`.
    stream_started_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    /// Resolves once `receive-via-stream` is called, so pending `receive` calls
    /// can be woken and fail with `receiving-via-stream`.
    stream_started: Shared<oneshot::Receiver<()>>,
    /// Keep the backing peer connection(s) alive for the channel's lifetime.
    /// Dropped in `Drop`, which also closes each connection so `webrtc-rs` tears
    /// down its ICE/DTLS/SCTP background tasks instead of leaking them.
    keep_alive: Vec<Arc<RTCPeerConnection>>,
}

impl DataChannel {
    /// Wrap an open data channel and its inbound-message receiver, retaining the
    /// given peer connections so they outlive the channel.
    pub fn new(
        channel: Arc<RTCDataChannel>,
        incoming: UnboundedReceiver<InboundMessage>,
        keep_alive: Vec<Arc<RTCPeerConnection>>,
    ) -> Self {
        let (started_tx, started_rx) = oneshot::channel();
        Self {
            channel,
            incoming: Arc::new(AsyncMutex::new(incoming)),
            stream_receiving: Arc::new(AtomicBool::new(false)),
            stream_started_tx: Arc::new(Mutex::new(Some(started_tx))),
            stream_started: started_rx.shared(),
            keep_alive,
        }
    }

    /// The negotiated channel label.
    pub fn label(&self) -> String {
        self.channel.label().to_string()
    }

    /// A cheap clone of the underlying `webrtc-rs` data channel.
    pub fn channel(&self) -> Arc<RTCDataChannel> {
        self.channel.clone()
    }

    /// A cheap clone of the shared inbound-message receiver, so an async `receive`
    /// can await the next message without holding the store borrow.
    pub fn incoming(&self) -> Arc<AsyncMutex<UnboundedReceiver<InboundMessage>>> {
        self.incoming.clone()
    }

    /// Claim the channel's inbound messages for `receive-via-stream`.
    ///
    /// Returns `true` for the first caller (which takes ownership of the inbound
    /// stream) and `false` for every later caller. On the first call it also
    /// wakes any pending `receive` calls so they can fail with
    /// `receiving-via-stream` before `receive-via-stream` returns.
    pub fn begin_stream_receiving(&self) -> bool {
        let mut guard = self.stream_started_tx.lock().unwrap();
        match guard.take() {
            Some(tx) => {
                self.stream_receiving.store(true, Ordering::SeqCst);
                let _ = tx.send(());
                true
            }
            None => false,
        }
    }

    /// Whether `receive-via-stream` has claimed the inbound messages.
    pub fn is_stream_receiving(&self) -> bool {
        self.stream_receiving.load(Ordering::SeqCst)
    }

    /// A future that resolves once `receive-via-stream` is called, used to wake
    /// pending `receive` calls.
    pub fn stream_started(&self) -> Shared<oneshot::Receiver<()>> {
        self.stream_started.clone()
    }
}

impl Drop for DataChannel {
    fn drop(&mut self) {
        close_peer_connections(std::mem::take(&mut self.keep_alive));
    }
}

/// Close each peer connection so `webrtc-rs` tears down its ICE/DTLS/SCTP
/// background tasks.
///
/// `RTCPeerConnection::close` is async, so the closes are spawned onto the
/// current Tokio runtime; dropping the `Arc`s alone would leak those tasks for
/// the process lifetime. Called from `Drop` impls, where awaiting is not
/// possible; if no runtime is running the connections are simply dropped.
pub fn close_peer_connections(connections: Vec<Arc<RTCPeerConnection>>) {
    if connections.is_empty() {
        return;
    }
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            for connection in connections {
                let _ = connection.close().await;
            }
        });
    }
}

/// Create a peer connection, giving the caller a chance to customize the
/// `webrtc-rs` [`SettingEngine`] first.
///
/// The crate deliberately does **not** hardcode any environment-driven ICE
/// tweaks. Instead, `configure` is run against a fresh [`SettingEngine`] before
/// the peer connection is built, mirroring the customization hook
/// wasmtime-wasi-http exposes via its `WasiHttpHooks`. Demo and test hosts use
/// this to, for example, opt into loopback ICE candidates when both peers share
/// one host (see [`WasiWebrtcCtx::set_setting_engine_hook`]); passing a no-op
/// closure yields host ICE candidates only.
///
/// [`SettingEngine`]: webrtc::api::setting_engine::SettingEngine
/// [`WasiWebrtcCtx::set_setting_engine_hook`]: crate::WasiWebrtcCtx::set_setting_engine_hook
pub async fn new_peer_connection(
    configure: impl FnOnce(&mut SettingEngine),
) -> Result<Arc<RTCPeerConnection>> {
    let media = MediaEngine::default();
    // Data channels don't need media codecs, but the API builder wants a media
    // engine; a default one is sufficient for SCTP.
    let registry = Registry::new();
    let mut setting = SettingEngine::default();
    configure(&mut setting);
    let api = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_setting_engine(setting)
        .build();
    let config = RTCConfiguration::default();
    Ok(Arc::new(api.new_peer_connection(config).await?))
}

/// Send one message over a data channel.
pub async fn send_message(channel: &Arc<RTCDataChannel>, message: Vec<u8>) -> Result<()> {
    channel.send(&Bytes::from(message)).await?;
    Ok(())
}
