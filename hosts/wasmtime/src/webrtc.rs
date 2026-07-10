//! The `webrtc-rs`-backed echo endpoint.
//!
//! `build_echo` stands up two `RTCPeerConnection`s, wires their ICE candidates
//! directly to each other, echoes every message received on the far side, and
//! returns the near data channel plus a `futures` stream of inbound messages.

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
pub struct EchoDataChannel {
    channel: Arc<RTCDataChannel>,
    /// Inbound messages, taken once by `receive`.
    incoming: Mutex<Option<UnboundedReceiver<Vec<u8>>>>,
    /// Keep the backing peer connection(s) alive for the channel's lifetime.
    _keep_alive: Vec<Arc<RTCPeerConnection>>,
}

impl EchoDataChannel {
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

    pub fn label(&self) -> String {
        self.channel.label().to_string()
    }

    pub fn channel(&self) -> Arc<RTCDataChannel> {
        self.channel.clone()
    }

    pub fn take_incoming(&self) -> Option<UnboundedReceiver<Vec<u8>>> {
        self.incoming.lock().unwrap().take()
    }
}

async fn new_peer_connection() -> Result<Arc<RTCPeerConnection>> {
    let mut media = MediaEngine::default();
    // Data channels don't need media codecs, but the API builder wants a
    // media engine; a default one is sufficient for SCTP.
    let _ = &mut media;
    let registry = Registry::new();
    let mut builder = APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry);
    // When two peers run on the *same* host (as in the local demos and tests),
    // their only mutually reachable addresses may be loopback. webrtc-rs omits
    // loopback candidates by default, so opt in when asked. Harmless for real
    // cross-host use (the loopback pair simply never succeeds there).
    if std::env::var_os("WEBRTC_INCLUDE_LOOPBACK").is_some() {
        let mut setting = SettingEngine::default();
        setting.set_include_loopback_candidate(true);
        builder = builder.with_setting_engine(setting);
    }
    let api = builder.build();
    let config = RTCConfiguration::default();
    Ok(Arc::new(api.new_peer_connection(config).await?))
}

/// Build an echo-connected near data channel.
pub async fn build_echo(
    label: &str,
    ordered: bool,
    max_retransmits: Option<u16>,
) -> Result<EchoDataChannel> {
    let near = new_peer_connection().await?;
    let far = new_peer_connection().await?;

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

    Ok(EchoDataChannel::new(channel, in_rx, vec![near, far]))
}

/// Create a peer connection with a default configuration (host ICE candidates
/// only — sufficient for two local processes). Shared by the echo endpoint and
/// the manual-signaling host.
pub async fn new_peer_connection_pub() -> Result<Arc<RTCPeerConnection>> {
    new_peer_connection().await
}

/// Send one message over a data channel.
pub async fn send_message(channel: &Arc<RTCDataChannel>, message: Vec<u8>) -> Result<()> {
    channel.send(&Bytes::from(message)).await?;
    Ok(())
}
