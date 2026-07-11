//! The `webrtc-rs`-backed [`DataChannel`] host resource and helpers.
//!
//! [`DataChannel`] is the concrete host type mapped onto the
//! `lann:webrtc-datachannels/data-channels.data-channel` resource. It wraps an
//! open `webrtc-rs` [`RTCDataChannel`] and the peer connection(s) that must
//! outlive it. The channel's inbound-message stream is produced once at
//! construction (see [`crate::inbound_stream`]) rather than stored here.

use std::sync::Arc;

use anyhow::Result;
use bytes::Bytes;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::RTCDataChannel;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;

/// Host state behind a `data-channel` resource.
///
/// A connected, bidirectional WebRTC data channel. The inbound-message stream
/// is handed back alongside the channel at construction time, so it is not
/// retained here.
pub struct DataChannel {
    channel: Arc<RTCDataChannel>,
    /// Keep the backing peer connection(s) alive for the channel's lifetime.
    _keep_alive: Vec<Arc<RTCPeerConnection>>,
}

impl DataChannel {
    /// Wrap an open data channel, retaining the given peer connections so they
    /// outlive the channel.
    pub fn new(channel: Arc<RTCDataChannel>, keep_alive: Vec<Arc<RTCPeerConnection>>) -> Self {
        Self {
            channel,
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
