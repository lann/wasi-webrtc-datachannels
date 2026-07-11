//! Test-only `manual-signaling` host implementation used by
//! `tests/manual_signaling.rs`.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::channel::oneshot;
use wasmtime::component::{Accessor, HasData, Linker, Resource, StreamReader};
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use wasmtime_webrtc_datachannels::{
    inbound_stream, new_peer_connection, DataChannel, SettingEngineHook, WasiWebrtcCtxView,
    WasiWebrtcView,
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../examples/cli-signaling/wit",
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
            "lann:webrtc-datachannels/data-channels.data-channel":
                wasmtime_webrtc_datachannels::DataChannel,
            "demo:webrtc-echo/manual-signaling.peer-connection": super::ManualPeer,
        },
    });
}

use bindings::demo::webrtc_echo::manual_signaling::{
    self, HostPeerConnection, HostPeerConnectionWithStore,
};
use bindings::lann::webrtc_datachannels::types::{DataChannelOptions, Error};

/// [`HasData`] marker for the demo-only `manual-signaling` host bindings.
struct ManualSignaling;

impl HasData for ManualSignaling {
    type Data<'a> = WasiWebrtcCtxView<'a>;
}

/// Add the demo-only `demo:webrtc-echo/manual-signaling` interface to `linker`.
///
/// The `data-channels`/`types` imports must be provided separately by
/// [`wasmtime_webrtc_datachannels::add_to_linker`]; the channels this
/// interface returns are that crate's [`DataChannel`].
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: WasiWebrtcView + 'static,
{
    manual_signaling::add_to_linker::<_, ManualSignaling>(linker, T::webrtc)?;
    Ok(())
}

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
    /// `RTCPeerConnection` is created on first use, with `setting_engine_hook`
    /// applied to its `SettingEngine`.
    pub fn new(setting_engine_hook: Option<SettingEngineHook>) -> Self {
        Self {
            pc: Arc::new(Mutex::new(None)),
            negotiated: Arc::new(Mutex::new(Negotiated::default())),
            channel_arrived: Arc::new(Mutex::new(None)),
            setting_engine_hook,
        }
    }

    /// Create the backing peer connection and store it, returning a handle.
    async fn init_pc(&self) -> Result<Arc<RTCPeerConnection>> {
        let hook = self.setting_engine_hook.clone();
        let pc = new_peer_connection(|engine| {
            if let Some(hook) = &hook {
                hook(engine);
            }
        })
        .await?;
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
    /// host resource paired with its inbound-message receiver.
    pub async fn connect(&self) -> Result<(DataChannel, UnboundedReceiver<Vec<u8>>)> {
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
        Ok((DataChannel::new(channel, vec![self.pc()?]), incoming))
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

// --- host trait implementations --------------------------------------------

impl manual_signaling::Host for WasiWebrtcCtxView<'_> {}

impl HostPeerConnection for WasiWebrtcCtxView<'_> {
    fn new(&mut self) -> wasmtime::Result<Resource<ManualPeer>> {
        let hook = self.ctx.setting_engine_hook();
        Ok(self.table.push(ManualPeer::new(hook))?)
    }

    fn close(&mut self, _self_: Resource<ManualPeer>) -> wasmtime::Result<()> {
        // The peer connection is torn down when the resource is dropped.
        Ok(())
    }
}

impl<T> HostPeerConnectionWithStore<T> for ManualSignaling {
    async fn create_offer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        options: DataChannelOptions,
    ) -> wasmtime::Result<std::result::Result<String, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
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
    ) -> wasmtime::Result<std::result::Result<(Resource<DataChannel>, StreamReader<Vec<u8>>), Error>>
    {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        match peer.connect().await {
            Ok((channel, incoming)) => {
                let resource = accessor.with(|mut access| access.get().table.push(channel))?;
                let stream = inbound_stream(accessor, incoming)?;
                Ok(Ok((resource, stream)))
            }
            Err(err) => Ok(Err(Error::Other(err.to_string()))),
        }
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<ManualPeer>) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
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
