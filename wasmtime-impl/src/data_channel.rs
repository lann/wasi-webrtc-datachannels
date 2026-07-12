//! The `webrtc-rs`-backed [`DataChannel`] host resource and helpers.
//!
//! [`DataChannel`] is the concrete host type mapped onto the
//! `lann:webrtc-datachannels/data-channels.data-channel` resource. It wraps an
//! open `webrtc-rs` [`RTCDataChannel`], its inbound-message stream, and the peer
//! connection(s) that must outlive it.

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use futures::channel::mpsc::UnboundedReceiver;
use futures::lock::Mutex as AsyncMutex;
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
    /// Keep the backing peer connection(s) alive for the channel's lifetime.
    _keep_alive: Vec<Arc<RTCPeerConnection>>,
}

impl DataChannel {
    /// Wrap an open data channel and its inbound-message receiver, retaining the
    /// given peer connections so they outlive the channel.
    pub fn new(
        channel: Arc<RTCDataChannel>,
        incoming: UnboundedReceiver<InboundMessage>,
        keep_alive: Vec<Arc<RTCPeerConnection>>,
    ) -> Self {
        Self {
            channel,
            incoming: Arc::new(AsyncMutex::new(incoming)),
            _keep_alive: keep_alive,
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
