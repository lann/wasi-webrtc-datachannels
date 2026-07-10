//! The `webrtc-rs`-backed manual-signaling peer connection.
//!
//! This backs the `wasi:webrtc-data-channels/manual-signaling` `peer-connection`
//! resource with a real `webrtc-rs` [`RTCPeerConnection`], using *vanilla*
//! (non-trickle) ICE: after applying a local description, we wait for ICE
//! gathering to complete and read back the local description, which then already
//! contains every gathered candidate. That is what lets the whole exchange be
//! just two complete SDP blobs (offer, answer).

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::channel::oneshot;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use crate::webrtc::new_peer_connection_pub;
use crate::EchoDataChannel;

/// Everything captured for the negotiated data channel.
#[derive(Default)]
struct Negotiated {
    channel: Option<Arc<RTCDataChannel>>,
    incoming: Option<UnboundedReceiver<Vec<u8>>>,
    /// Resolves once the channel reports `open`. A oneshot (rather than a bare
    /// notify) so an early open is not missed if `connect` awaits later.
    open: Option<oneshot::Receiver<()>>,
}

/// Host state behind a manual-signaling `peer-connection` resource.
///
/// All fields are behind `Arc`, so a handle can be cheaply cloned out of the
/// resource table and its async methods driven without holding the store borrow
/// across `.await`.
#[derive(Clone)]
pub struct ManualPeer {
    /// Created lazily on the first `create-offer`/`create-answer` call, because
    /// the WIT `constructor` is synchronous but building a `webrtc-rs` peer
    /// connection is async.
    pc: Arc<Mutex<Option<Arc<RTCPeerConnection>>>>,
    negotiated: Arc<Mutex<Negotiated>>,
    /// (Answerer only) resolves once the offerer's data channel has arrived via
    /// `on_data_channel` and populated `negotiated`. `None` for the offerer,
    /// which creates its channel synchronously in `create_offer`.
    channel_arrived: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
}

impl Default for ManualPeer {
    fn default() -> Self {
        Self::new()
    }
}

impl ManualPeer {
    /// Construct an uninitialized peer (the resource `constructor`). The backing
    /// `RTCPeerConnection` is created on first use.
    pub fn new() -> Self {
        Self {
            pc: Arc::new(Mutex::new(None)),
            negotiated: Arc::new(Mutex::new(Negotiated::default())),
            channel_arrived: Arc::new(Mutex::new(None)),
        }
    }

    /// Create the backing peer connection and store it, returning a handle.
    async fn init_pc(&self) -> Result<Arc<RTCPeerConnection>> {
        let pc = new_peer_connection_pub().await?;
        if std::env::var_os("WEBRTC_SIGNALING_DEBUG").is_some() {
            pc.on_ice_connection_state_change(Box::new(|state| {
                eprintln!("[manual] ice-connection-state: {state}");
                Box::pin(async {})
            }));
            pc.on_peer_connection_state_change(Box::new(|state| {
                eprintln!("[manual] peer-connection-state: {state}");
                Box::pin(async {})
            }));
        }
        *self.pc.lock().unwrap() = Some(pc.clone());
        Ok(pc)
    }

    /// Return the backing peer connection, erroring if signaling has not started.
    fn pc(&self) -> Result<Arc<RTCPeerConnection>> {
        self.pc
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("peer connection has not been initialized"))
    }

    /// (Offerer) Create the data channel, produce a complete SDP offer with all
    /// ICE candidates gathered, and return it.
    pub async fn create_offer(
        &self,
        label: &str,
        ordered: bool,
        max_retransmits: Option<u16>,
    ) -> Result<String> {
        let init = RTCDataChannelInit {
            ordered: Some(ordered),
            max_retransmits,
            ..Default::default()
        };
        let pc = self.init_pc().await?;
        let channel = pc.create_data_channel(label, Some(init)).await?;
        let negotiated = wire_channel(&channel);
        *self.negotiated.lock().unwrap() = negotiated;

        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer).await?;
        self.await_gathering(&pc).await;
        local_sdp(&pc).await
    }

    /// (Offerer) Apply the peer's complete SDP answer.
    pub async fn accept_answer(&self, answer_sdp: String) -> Result<()> {
        let answer = RTCSessionDescription::answer(answer_sdp)?;
        self.pc()?.set_remote_description(answer).await?;
        Ok(())
    }

    /// (Answerer) Apply the peer's complete SDP offer, produce a complete SDP
    /// answer with all ICE candidates gathered, and return it.
    pub async fn create_answer(&self, offer_sdp: String) -> Result<String> {
        let pc = self.init_pc().await?;

        // The offerer's data channel arrives via `on_data_channel` some time
        // after the connection opens; capture it and signal its arrival so
        // `connect` can wait for it.
        let (arrived_tx, arrived_rx) = oneshot::channel::<()>();
        *self.channel_arrived.lock().unwrap() = Some(arrived_rx);
        let arrived_tx = Arc::new(Mutex::new(Some(arrived_tx)));
        let slot = self.negotiated.clone();
        pc.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
            let slot = slot.clone();
            let arrived_tx = arrived_tx.clone();
            Box::pin(async move {
                let negotiated = wire_channel(&channel);
                *slot.lock().unwrap() = negotiated;
                if let Some(tx) = arrived_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            })
        }));

        let offer = RTCSessionDescription::offer(offer_sdp)?;
        pc.set_remote_description(offer).await?;
        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer).await?;
        self.await_gathering(&pc).await;
        local_sdp(&pc).await
    }

    /// Wait until the data channel is open and return it as a `data-channel`
    /// host resource.
    pub async fn connect(&self) -> Result<EchoDataChannel> {
        // (Answerer) wait until `on_data_channel` has delivered the channel.
        let arrived = self.channel_arrived.lock().unwrap().take();
        if let Some(arrived) = arrived {
            let _ = arrived.await;
        }

        let open = self.negotiated.lock().unwrap().open.take();
        match open {
            Some(open) => {
                let _ = open.await;
            }
            None => {
                return Err(anyhow!(
                    "connect() called before signaling produced a channel"
                ))
            }
        }

        let mut negotiated = self.negotiated.lock().unwrap();
        let channel = negotiated
            .channel
            .clone()
            .ok_or_else(|| anyhow!("no data channel was negotiated"))?;
        let incoming = negotiated
            .incoming
            .take()
            .ok_or_else(|| anyhow!("data channel has no inbound stream"))?;
        Ok(EchoDataChannel::new(channel, incoming, vec![self.pc()?]))
    }

    /// Block until ICE gathering has completed so the local description carries
    /// every candidate.
    async fn await_gathering(&self, pc: &Arc<RTCPeerConnection>) {
        let mut gather_complete = pc.gathering_complete_promise().await;
        let _ = gather_complete.recv().await;
    }
}

/// Read back a peer connection's complete local description (with candidates).
async fn local_sdp(pc: &Arc<RTCPeerConnection>) -> Result<String> {
    let description = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no local description available"))?;
    Ok(description.sdp)
}

/// Attach open/message handlers to `channel` and return its negotiated state
/// (the channel, its inbound-message receiver, and an open signal).
fn wire_channel(channel: &Arc<RTCDataChannel>) -> Negotiated {
    let (in_tx, in_rx) = mpsc::unbounded::<Vec<u8>>();
    channel.on_message(Box::new(move |message: DataChannelMessage| {
        let in_tx = in_tx.clone();
        Box::pin(async move {
            let _ = in_tx.unbounded_send(message.data.to_vec());
        })
    }));

    let (open_tx, open_rx) = oneshot::channel::<()>();
    let open_tx = Arc::new(Mutex::new(Some(open_tx)));
    channel.on_open(Box::new(move || {
        let open_tx = open_tx.clone();
        Box::pin(async move {
            if let Some(tx) = open_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        })
    }));

    Negotiated {
        channel: Some(channel.clone()),
        incoming: Some(in_rx),
        open: Some(open_rx),
    }
}

/// Send one message over a data channel.
pub async fn send_message(channel: &Arc<RTCDataChannel>, message: Vec<u8>) -> Result<()> {
    channel.send(&Bytes::from(message)).await?;
    Ok(())
}
