//! The `webrtc-rs`-backed [`DataChannel`] host resource and helpers.
//!
//! [`DataChannel`] is the concrete host type mapped onto the
//! `lann:webrtc-datachannels/data-channels.data-channel` resource. It wraps an
//! open `webrtc-rs` [`RTCDataChannel`], its inbound-message stream, and the peer
//! connection(s) that must outlive it.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use bytes::Bytes;
use futures::channel::mpsc::UnboundedReceiver;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;

/// Host state behind a `data-channel` resource.
///
/// A connected, bidirectional WebRTC data channel plus the inbound-message
/// stream consumed once by `receive`.
pub struct DataChannel {
    channel: Arc<RTCDataChannel>,
    /// Inbound messages, taken once by `receive`.
    incoming: Mutex<Option<UnboundedReceiver<Vec<u8>>>>,
    /// Keep the backing peer connection(s) alive for the channel's lifetime.
    _keep_alive: Vec<Arc<RTCPeerConnection>>,
}

impl DataChannel {
    /// Wrap an open data channel and its inbound-message receiver, retaining the
    /// given peer connections so they outlive the channel.
    pub fn new(
        channel: Arc<RTCDataChannel>,
        incoming: UnboundedReceiver<Vec<u8>>,
        keep_alive: Vec<Arc<RTCPeerConnection>>,
    ) -> Self {
        Self {
            channel,
            incoming: Mutex::new(Some(incoming)),
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

    /// Take the inbound-message receiver. Returns `None` after the first call.
    pub fn take_incoming(&self) -> Option<UnboundedReceiver<Vec<u8>>> {
        self.incoming.lock().unwrap().take()
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
