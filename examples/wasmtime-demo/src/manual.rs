//! Demo-only `manual-signaling` host implementation, backed by `webrtc-rs`.
//!
//! `manual-signaling` is a demo-only interface (`demo:webrtc-echo`), so its host
//! implementation lives here rather than in the
//! `wasmtime-webrtc-datachannels` crate. It backs the `peer-connection`
//! resource with a real `webrtc-rs` peer connection, using *vanilla*
//! (non-trickle) ICE: after applying a local description, we wait for ICE
//! gathering to complete and read back the local description, which then already
//! contains every gathered candidate. That is what lets the whole exchange be
//! just two complete SDP blobs (offer, answer).
//!
//! Data channels it hands back are the crate's [`DataChannel`], so the
//! same crate `add_to_linker` that satisfies `connections` also drives the
//! `send`/`receive` on channels this interface produces.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use futures::channel::oneshot;
use wasmtime::component::{Accessor, HasData, Linker, Resource};
use webrtc::data_channel::{DataChannel as WebrtcDataChannel, RTCDataChannelInit};
use webrtc::peer_connection::{PeerConnection, RTCSessionDescription};

use wasmtime_webrtc_datachannels::{
    close_peer_connections, new_peer_connection, spawn_channel_pump, CallbackHandler, DataChannel,
    InboundQueue, SettingEngineHook, WasiWebrtcCtxView, WasiWebrtcView,
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../cli-signaling/wit",
        world: "manual-signaling-host",
        imports: {
            // The async signaling methods use the component-model async ABI and
            // need `Accessor` access to the store.
            default: async | store | trappable,
            // A resource `constructor` cannot use the async ABI, and `close` is
            // a synchronous WIT function, so both are bound synchronously.
            "demo:webrtc-echo/manual-signaling@0.1.0.[constructor]peer-connection": trappable,
            "demo:webrtc-echo/manual-signaling@0.1.0.[method]peer-connection.close": trappable,
        },
        with: {
            "lann:webrtc-datachannels/connections.data-channel-options":
                wasmtime_webrtc_datachannels::DataChannelOptions,
            "lann:webrtc-datachannels/connections.data-channel":
                wasmtime_webrtc_datachannels::DataChannel,
            "demo:webrtc-echo/manual-signaling.peer-connection": super::ManualPeer,
        },
    });
}

use bindings::demo::webrtc_echo::manual_signaling::{
    self, HostPeerConnection, HostPeerConnectionWithStore,
};
use bindings::lann::webrtc_datachannels::types::Error;
use wasmtime_webrtc_datachannels::DataChannelOptions;

/// [`HasData`] marker for the demo-only `manual-signaling` host bindings.
struct ManualSignaling;

impl HasData for ManualSignaling {
    type Data<'a> = WasiWebrtcCtxView<'a>;
}

/// Add the demo-only `demo:webrtc-echo/manual-signaling` interface to `linker`.
///
/// The `connections`/`types` imports must be provided separately by
/// [`wasmtime_webrtc_datachannels::add_to_linker`]; the channels this
/// interface returns are that crate's [`DataChannel`].
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: WasiWebrtcView + 'static,
{
    manual_signaling::add_to_linker::<_, ManualSignaling>(linker, T::webrtc)?;
    Ok(())
}

/// A callback invoked with each data channel opened by the remote peer.
type OnDataChannel = Box<dyn Fn(Arc<dyn WebrtcDataChannel>) + Send + Sync>;

/// Everything captured for the negotiated data channel.
#[derive(Default)]
struct Negotiated {
    /// The negotiated channel label (the offerer knows it up front; the answerer
    /// resolves it from the arriving channel, whose accessor is async).
    label: String,
    channel: Option<Arc<dyn WebrtcDataChannel>>,
    incoming: Option<InboundQueue>,
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
    pc: Arc<Mutex<Option<Arc<dyn PeerConnection>>>>,
    negotiated: Arc<Mutex<Negotiated>>,
    /// (Answerer only) resolves once the offerer's data channel has arrived via
    /// `on_data_channel` and populated `negotiated`. `None` for the offerer,
    /// which creates its channel synchronously in `create_offer`.
    channel_arrived: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    /// Resolves once ICE gathering reports `complete`, so the local description
    /// read back afterwards already carries every candidate. Set up in
    /// `init_pc` and awaited by `await_gathering`.
    gather_complete: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    /// Hook applied to each peer connection's `SettingEngine` (e.g. to enable
    /// loopback ICE candidates for two same-host peers).
    setting_engine_hook: Option<SettingEngineHook>,
}

impl Default for ManualPeer {
    fn default() -> Self {
        Self::new(None)
    }
}

impl ManualPeer {
    /// Construct an uninitialized peer (the resource `constructor`). The backing
    /// peer connection is created on first use, with `setting_engine_hook`
    /// applied to its `SettingEngine`.
    pub fn new(setting_engine_hook: Option<SettingEngineHook>) -> Self {
        Self {
            pc: Arc::new(Mutex::new(None)),
            negotiated: Arc::new(Mutex::new(Negotiated::default())),
            channel_arrived: Arc::new(Mutex::new(None)),
            gather_complete: Arc::new(Mutex::new(None)),
            setting_engine_hook,
        }
    }

    /// Create the backing peer connection and store it, returning a handle.
    ///
    /// The `webrtc` 0.20 builder takes one event handler at build time, so the
    /// callbacks are assembled here: an ICE-gathering-complete signal feeding
    /// [`ManualPeer::await_gathering`], the answerer's optional `on_data_channel`
    /// (which wires the arriving channel), and debug state logging.
    async fn init_pc(
        &self,
        on_data_channel: Option<OnDataChannel>,
    ) -> Result<Arc<dyn PeerConnection>> {
        let hook = self.setting_engine_hook.clone();

        let (gather_tx, gather_rx) = oneshot::channel::<()>();
        *self.gather_complete.lock().unwrap() = Some(gather_rx);
        let gather_tx = Arc::new(Mutex::new(Some(gather_tx)));

        let mut handler = CallbackHandler::new().on_gathering_complete(move || {
            if let Some(tx) = gather_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        });
        if let Some(callback) = on_data_channel {
            handler = handler.on_data_channel(callback);
        }
        if std::env::var_os("WEBRTC_SIGNALING_DEBUG").is_some() {
            handler = handler
                .on_ice_connection_state(|state| {
                    eprintln!("[manual] ice-connection-state: {state}");
                })
                .on_connection_state(|state| {
                    eprintln!("[manual] peer-connection-state: {state}");
                });
        }

        let pc = new_peer_connection(
            |engine| {
                if let Some(hook) = &hook {
                    hook(engine);
                }
            },
            Arc::new(handler),
        )
        .await?;
        *self.pc.lock().unwrap() = Some(pc.clone());
        Ok(pc)
    }

    /// Return the backing peer connection, erroring if signaling has not started.
    fn pc(&self) -> Result<Arc<dyn PeerConnection>> {
        self.pc
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("peer connection has not been initialized"))
    }

    /// Close the backing peer connection, tearing down its `webrtc-rs`
    /// background tasks. Idempotent: the connection is taken out of its slot, so
    /// a second call (e.g. from both `close` and resource drop) is a no-op.
    pub fn close(&self) {
        let connection = self.pc.lock().unwrap().take();
        close_peer_connections(connection.into_iter().collect());
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
            ordered,
            max_retransmits,
            ..Default::default()
        };
        let pc = self.init_pc(None).await?;
        let channel = pc.create_data_channel(label, Some(init)).await?;
        let negotiated = wire_channel(label.to_string(), &channel);
        *self.negotiated.lock().unwrap() = negotiated;

        let offer = pc.create_offer(None).await?;
        pc.set_local_description(offer).await?;
        self.await_gathering().await;
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
        // The offerer's data channel arrives via `on_data_channel` some time
        // after the connection opens; capture it and signal its arrival so
        // `connect` can wait for it. The channel's label accessor is async, so
        // wiring runs in a spawned task that also fills the `negotiated` slot.
        let (arrived_tx, arrived_rx) = oneshot::channel::<()>();
        *self.channel_arrived.lock().unwrap() = Some(arrived_rx);
        let arrived_tx = Arc::new(Mutex::new(Some(arrived_tx)));
        let slot = self.negotiated.clone();
        let on_data_channel = move |channel: Arc<dyn WebrtcDataChannel>| {
            let slot = slot.clone();
            let arrived_tx = arrived_tx.clone();
            tokio::spawn(async move {
                let label = channel.label().await.unwrap_or_default();
                let negotiated = wire_channel(label, &channel);
                *slot.lock().unwrap() = negotiated;
                if let Some(tx) = arrived_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            });
        };

        let pc = self.init_pc(Some(Box::new(on_data_channel))).await?;

        let offer = RTCSessionDescription::offer(offer_sdp)?;
        pc.set_remote_description(offer).await?;
        let answer = pc.create_answer(None).await?;
        pc.set_local_description(answer).await?;
        self.await_gathering().await;
        local_sdp(&pc).await
    }

    /// Wait until the data channel is open and return it as a `data-channel`
    /// host resource.
    pub async fn connect(&self) -> Result<DataChannel> {
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
        let label = negotiated.label.clone();
        let channel = negotiated
            .channel
            .clone()
            .ok_or_else(|| anyhow!("no data channel was negotiated"))?;
        let incoming = negotiated
            .incoming
            .take()
            .ok_or_else(|| anyhow!("data channel has no inbound stream"))?;
        Ok(DataChannel::new(label, channel, incoming, vec![self.pc()?]))
    }

    /// Block until ICE gathering has completed so the local description carries
    /// every candidate.
    async fn await_gathering(&self) {
        let gather = self.gather_complete.lock().unwrap().take();
        if let Some(gather) = gather {
            let _ = gather.await;
        }
    }
}

/// Read back a peer connection's complete local description (with candidates).
async fn local_sdp(pc: &Arc<dyn PeerConnection>) -> Result<String> {
    let description = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow!("no local description available"))?;
    Ok(description.sdp)
}

/// Spawn `channel`'s pump task and return its negotiated state (the channel, its
/// inbound-message receiver, and an open signal).
fn wire_channel(label: String, channel: &Arc<dyn WebrtcDataChannel>) -> Negotiated {
    let pump = spawn_channel_pump(channel.clone());
    Negotiated {
        label,
        channel: Some(channel.clone()),
        incoming: Some(pump.incoming),
        open: Some(pump.open),
    }
}

// --- host trait implementations --------------------------------------------

impl manual_signaling::Host for WasiWebrtcCtxView<'_> {}

impl HostPeerConnection for WasiWebrtcCtxView<'_> {
    fn new(&mut self) -> wasmtime::Result<Resource<ManualPeer>> {
        let hook = self.ctx.setting_engine_hook();
        Ok(self.table.push(ManualPeer::new(hook))?)
    }

    fn close(&mut self, self_: Resource<ManualPeer>) -> wasmtime::Result<()> {
        self.table.get(&self_)?.close();
        Ok(())
    }
}

impl<T> HostPeerConnectionWithStore<T> for ManualSignaling {
    async fn create_offer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        options: Resource<DataChannelOptions>,
    ) -> wasmtime::Result<std::result::Result<String, Error>> {
        let (peer, options) = accessor.with(|mut access| {
            let peer = clone_peer(access.get(), &self_)?;
            let options = access.get().table.delete(options)?;
            Ok::<_, wasmtime::Error>((peer, options))
        })?;
        Ok(peer
            .create_offer(&options.label, options.ordered, options.max_retransmits)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn accept_answer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        answer: String,
    ) -> wasmtime::Result<std::result::Result<(), Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        Ok(peer
            .accept_answer(answer)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn create_answer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        offer: String,
    ) -> wasmtime::Result<std::result::Result<String, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        Ok(peer
            .create_answer(offer)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn connect(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
    ) -> wasmtime::Result<std::result::Result<Resource<DataChannel>, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        match peer.connect().await {
            Ok(channel) => {
                let resource = accessor.with(|mut access| access.get().table.push(channel))?;
                Ok(Ok(resource))
            }
            Err(err) => Ok(Err(Error::Other(err.to_string()))),
        }
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<ManualPeer>) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            let peer = access.get().table.delete(rep)?;
            peer.close();
            Ok(())
        })
    }
}

/// Clone the cheaply-`Arc`-backed [`ManualPeer`] out of the table so its async
/// methods can run without holding the store borrow across `.await`.
fn clone_peer(
    view: WasiWebrtcCtxView<'_>,
    self_: &Resource<ManualPeer>,
) -> wasmtime::Result<ManualPeer> {
    Ok(view.table.get(self_)?.clone())
}
