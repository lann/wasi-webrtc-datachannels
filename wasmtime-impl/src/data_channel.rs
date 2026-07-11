//! The `webrtc-rs`-backed [`DataChannel`] host resource and helpers.
//!
//! [`DataChannel`] is the concrete host type mapped onto the
//! `wasi:webrtc-data-channels/data-channels.data-channel` resource. It wraps an
//! open `webrtc-rs` [`RTCDataChannel`], its inbound-message stream, and the peer
//! connection(s) that must outlive it.
//!
//! [`build_echo`] is a convenience used by the demo hosts (and the crate's own
//! integration tests): it stands up two `RTCPeerConnection`s, wires their ICE
//! candidates directly to each other, echoes every message received on the far
//! side, and returns the near [`DataChannel`].

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::channel::mpsc::{self, UnboundedReceiver};
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
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

/// Build an echo-connected near [`DataChannel`].
///
/// Stands up two `RTCPeerConnection`s, trickles ICE between them directly,
/// echoes every message received on the far side back on the same channel, and
/// returns the near side (the channel the guest drives). Both peers live inside
/// this process, so no external signaling is involved. `configure` is applied to
/// each peer's [`SettingEngine`] (see [`new_peer_connection`]); the two local
/// peers usually need loopback ICE candidates enabled through it.
pub async fn build_echo(
    label: &str,
    ordered: bool,
    max_retransmits: Option<u16>,
    configure: impl Fn(&mut SettingEngine),
) -> Result<DataChannel> {
    let near = new_peer_connection(&configure).await?;
    let far = new_peer_connection(&configure).await?;

    // Trickle ICE candidates directly between the two local peers.
    let far_for_ice = far.clone();
    near.on_ice_candidate(Box::new(move |candidate| {
        let far = far_for_ice.clone();
        Box::pin(async move {
            if let Some(candidate) = candidate {
                if let Ok(init) = candidate.to_json() {
                    let _ = far.add_ice_candidate(init).await;
                }
            }
        })
    }));
    let near_for_ice = near.clone();
    far.on_ice_candidate(Box::new(move |candidate| {
        let near = near_for_ice.clone();
        Box::pin(async move {
            if let Some(candidate) = candidate {
                if let Ok(init) = candidate.to_json() {
                    let _ = near.add_ice_candidate(init).await;
                }
            }
        })
    }));

    // Far side: echo every message straight back on the same channel.
    far.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        Box::pin(async move {
            let echo_channel = channel.clone();
            channel.on_message(Box::new(move |message: DataChannelMessage| {
                let echo_channel = echo_channel.clone();
                Box::pin(async move {
                    let _ = echo_channel.send(&message.data).await;
                })
            }));
        })
    }));

    // Near side: create the data channel the guest will drive.
    let init = RTCDataChannelInit {
        ordered: Some(ordered),
        max_retransmits,
        ..Default::default()
    };
    let channel = near.create_data_channel(label, Some(init)).await?;

    // Inbound messages -> a futures stream consumed by `receive`.
    let (in_tx, in_rx) = mpsc::unbounded::<Vec<u8>>();
    channel.on_message(Box::new(move |message: DataChannelMessage| {
        let in_tx = in_tx.clone();
        Box::pin(async move {
            let _ = in_tx.unbounded_send(message.data.to_vec());
        })
    }));

    // Signal when the channel is open.
    let (open_tx, open_rx) = futures::channel::oneshot::channel::<()>();
    let open_tx = Arc::new(Mutex::new(Some(open_tx)));
    channel.on_open(Box::new(move || {
        let open_tx = open_tx.clone();
        Box::pin(async move {
            if let Some(tx) = open_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        })
    }));

    // Standard offer/answer exchange.
    let offer = near.create_offer(None).await?;
    near.set_local_description(offer.clone()).await?;
    far.set_remote_description(offer).await?;
    let answer = far.create_answer(None).await?;
    far.set_local_description(answer.clone()).await?;
    near.set_remote_description(answer).await?;

    open_rx
        .await
        .map_err(|_| anyhow!("data channel closed before opening"))?;

    Ok(DataChannel::new(channel, in_rx, vec![near, far]))
}

/// Send one message over a data channel.
pub async fn send_message(channel: &Arc<RTCDataChannel>, message: Vec<u8>) -> Result<()> {
    channel.send(&Bytes::from(message)).await?;
    Ok(())
}
