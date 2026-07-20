//! The `webrtc-rs`-backed [`DataChannel`] host resource and helpers.
//!
//! [`DataChannel`] is the concrete host type mapped onto the
//! `lann:webrtc-datachannels/connections.data-channel` resource. It wraps an
//! open `webrtc-rs` data channel, its inbound-message stream, and the peer
//! connection(s) that must outlive it.
//!
//! A channel may be **wired** immediately (the echo/manual hosts build the
//! `webrtc-rs` channel before constructing the resource) or **deferred** (the
//! `peer-connection` resource's synchronous `create-data-channel` hands back a
//! resource right away, then wires it once the peer connection has been built
//! and the channel opened). Both share the same [`DataChannel`] type; the async
//! methods await [`DataChannel::wired`] before touching the transport.
//!
//! The `webrtc` 0.20 data channel has no `on_open`/`on_message` callbacks;
//! instead each channel is driven by a per-channel **pump** task that loops on
//! [`webrtc::data_channel::DataChannel::poll`] and turns its
//! [`DataChannelEvent`]s into an open signal plus a stream of
//! [`InboundMessage`]s. Because every message (including a zero-length payload)
//! arrives as an `OnMessage` event rather than being conflated with
//! end-of-stream, empty messages are delivered to the guest.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use futures::channel::mpsc::{self, UnboundedReceiver};
use futures::channel::oneshot;
use futures::future::Shared;
use futures::lock::Mutex as AsyncMutex;
use futures::{FutureExt, TryFutureExt};
use webrtc::data_channel::{DataChannel as WebrtcDataChannel, DataChannelEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceServer, RTCIceTransportPolicy, SettingEngine,
};
use webrtc::runtime::default_runtime;

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

/// Why a channel's wiring failed. Cloneable so it can flow through the shared
/// wiring future to every awaiting `send`/`receive`.
#[derive(Clone, Debug)]
pub enum ChannelError {
    /// The channel closed (or its peer connection was torn down) before it
    /// could be wired.
    Closed,
    /// Wiring the channel failed for an implementation-specific reason.
    Other(String),
}

/// The transport-level parts of a wired channel: the open `webrtc-rs` channel
/// and its shared inbound-message receiver. Cheaply cloneable so it can be the
/// resolved value of the shared wiring future.
#[derive(Clone)]
pub struct Wired {
    /// The open `webrtc-rs` data channel.
    pub channel: Arc<dyn WebrtcDataChannel>,
    /// Inbound messages, delivered one per `receive` call. Behind an async mutex
    /// so concurrent receivers serialize and each takes the next message.
    pub incoming: Arc<AsyncMutex<UnboundedReceiver<InboundMessage>>>,
}

/// A future resolving to a channel's wired transport parts (or a wiring error),
/// shared so every awaiting async method observes the same outcome.
pub type WiredFuture = Shared<Pin<Box<dyn Future<Output = Result<Wired, ChannelError>> + Send>>>;

/// Build a [`WiredFuture`] that is already resolved to `wired`.
fn ready_wired(wired: Wired) -> WiredFuture {
    let fut: Pin<Box<dyn Future<Output = Result<Wired, ChannelError>> + Send>> =
        Box::pin(futures::future::ready(Ok(wired)));
    fut.shared()
}

/// The open signal and inbound-message stream produced by a channel's pump task.
pub struct ChannelPump {
    /// Inbound messages drained from the channel, in arrival order.
    pub incoming: UnboundedReceiver<InboundMessage>,
    /// Resolves once the channel reports `open`.
    pub open: oneshot::Receiver<()>,
}

/// Spawn the per-channel pump task that drives a `webrtc` 0.20 data channel.
///
/// The task loops on [`webrtc::data_channel::DataChannel::poll`] and translates
/// its [`DataChannelEvent`]s: `OnOpen` fires the open signal, each `OnMessage`
/// (including a zero-length payload) is forwarded as an [`InboundMessage`], and
/// `OnClose` (or a `None` poll) ends the pump, dropping the inbound sender so
/// receivers observe end-of-stream.
pub fn spawn_channel_pump(channel: Arc<dyn WebrtcDataChannel>) -> ChannelPump {
    let (in_tx, in_rx) = mpsc::unbounded::<InboundMessage>();
    let (open_tx, open_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut open_tx = Some(open_tx);
        while let Some(event) = channel.poll().await {
            match event {
                DataChannelEvent::OnOpen => {
                    if let Some(tx) = open_tx.take() {
                        let _ = tx.send(());
                    }
                }
                DataChannelEvent::OnMessage(message) => {
                    let _ = in_tx.unbounded_send(InboundMessage {
                        is_string: message.is_string,
                        data: message.data.to_vec(),
                    });
                }
                DataChannelEvent::OnClose => break,
                _ => {}
            }
        }
    });
    ChannelPump {
        incoming: in_rx,
        open: open_rx,
    }
}

/// Drive an open (or soon-to-open) channel into an existing wiring `oneshot`,
/// fulfilling it with the channel's transport parts once it opens, or
/// [`ChannelError::Closed`] if it closes first.
pub fn spawn_channel_wiring(
    channel: Arc<dyn WebrtcDataChannel>,
    wire_tx: oneshot::Sender<Result<Wired, ChannelError>>,
) {
    let pump = spawn_channel_pump(channel.clone());
    let incoming = Arc::new(AsyncMutex::new(pump.incoming));
    tokio::spawn(async move {
        match pump.open.await {
            Ok(()) => {
                let _ = wire_tx.send(Ok(Wired { channel, incoming }));
            }
            Err(_) => {
                let _ = wire_tx.send(Err(ChannelError::Closed));
            }
        }
    });
}

/// Wire an open (or soon-to-open) channel into a [`WiredFuture`] that resolves
/// with the channel's transport parts once it opens, or [`ChannelError::Closed`]
/// if it closes first. Used by the `peer-connection` resource's deferred and
/// remote-opened channel paths.
pub fn wire_open_channel(channel: Arc<dyn WebrtcDataChannel>) -> WiredFuture {
    let (wire_tx, wired) = wiring_channel();
    spawn_channel_wiring(channel, wire_tx);
    wired
}

/// Host state behind a `data-channel` resource.
///
/// A connected (or soon-to-be-connected), bidirectional WebRTC data channel plus
/// its inbound-message stream. The `receive-via-stream` claim machinery lives
/// here (not in [`Wired`]) so `receive-via-stream` can be claimed synchronously
/// even before the channel has finished wiring.
pub struct DataChannel {
    /// The negotiated label, known as soon as the resource is created (the
    /// deferred path takes it from the `data-channel-options`).
    label: String,
    /// Resolves to the channel's transport parts once it is wired.
    wired: WiredFuture,
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
    /// down its ICE/DTLS/SCTP background tasks instead of leaking them. Empty on
    /// the deferred `peer-connection` path, where the `peer-connection` resource
    /// owns and closes the connection.
    keep_alive: Vec<Arc<dyn PeerConnection>>,
}

impl DataChannel {
    /// Wrap an already-open data channel and its inbound-message receiver,
    /// retaining the given peer connections so they outlive the channel. Used by
    /// the echo/manual hosts, which build the channel before the resource. The
    /// `label` is supplied by the caller (the `webrtc` 0.20 channel exposes it
    /// only through an async accessor).
    pub fn new(
        label: String,
        channel: Arc<dyn WebrtcDataChannel>,
        incoming: UnboundedReceiver<InboundMessage>,
        keep_alive: Vec<Arc<dyn PeerConnection>>,
    ) -> Self {
        let wired = ready_wired(Wired {
            channel,
            incoming: Arc::new(AsyncMutex::new(incoming)),
        });
        Self::from_parts(label, wired, keep_alive)
    }

    /// Create a channel whose transport is wired later (the synchronous
    /// `peer-connection` `create-data-channel` path). `label` is known up front;
    /// `wired` resolves once the peer connection has built and opened the
    /// channel. The `peer-connection` resource owns the backing connection, so no
    /// `keep_alive` is retained here.
    pub fn deferred(label: String, wired: WiredFuture) -> Self {
        Self::from_parts(label, wired, Vec::new())
    }

    fn from_parts(
        label: String,
        wired: WiredFuture,
        keep_alive: Vec<Arc<dyn PeerConnection>>,
    ) -> Self {
        let (started_tx, started_rx) = oneshot::channel();
        Self {
            label,
            wired,
            stream_receiving: Arc::new(AtomicBool::new(false)),
            stream_started_tx: Arc::new(Mutex::new(Some(started_tx))),
            stream_started: started_rx.shared(),
            keep_alive,
        }
    }

    /// The negotiated channel label.
    pub fn label(&self) -> String {
        self.label.clone()
    }

    /// A clone of the shared wiring future, so an async method can await the
    /// channel's transport parts without holding the store borrow.
    pub fn wired(&self) -> WiredFuture {
        self.wired.clone()
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

/// Build a [`WiredFuture`] from a `oneshot` receiver, returning the sender the
/// wiring task fulfills. If the sender is dropped (the peer connection was torn
/// down before the channel opened), the future resolves to
/// [`ChannelError::Closed`].
pub fn wiring_channel() -> (oneshot::Sender<Result<Wired, ChannelError>>, WiredFuture) {
    let (tx, rx) = oneshot::channel::<Result<Wired, ChannelError>>();
    let fut: Pin<Box<dyn Future<Output = Result<Wired, ChannelError>> + Send>> =
        Box::pin(rx.unwrap_or_else(|_| Err(ChannelError::Closed)));
    (tx, fut.shared())
}

impl Drop for DataChannel {
    fn drop(&mut self) {
        close_peer_connections(std::mem::take(&mut self.keep_alive));
    }
}

/// Close each peer connection so `webrtc-rs` tears down its ICE/DTLS/SCTP
/// background tasks.
///
/// [`PeerConnection::close`] is async, so the closes are spawned onto the
/// current Tokio runtime; dropping the `Arc`s alone would leak those tasks for
/// the process lifetime. Called from `Drop` impls, where awaiting is not
/// possible; if no runtime is running the connections are simply dropped.
pub fn close_peer_connections(connections: Vec<Arc<dyn PeerConnection>>) {
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
/// `webrtc-rs` [`SettingEngine`] first and supplying the event `handler` that
/// receives its callbacks (ICE candidates, remote data channels, connection
/// state). The `webrtc` 0.20 builder takes a single
/// [`PeerConnectionEventHandler`] at build time, so callbacks cannot be attached
/// after construction.
///
/// The crate deliberately does **not** hardcode any environment-driven ICE
/// tweaks. Instead, `configure` is run against a fresh [`SettingEngine`] before
/// the peer connection is built, mirroring the customization hook
/// wasmtime-wasi-http exposes via its `WasiHttpHooks`.
///
/// Peers bind on IPv4 loopback (`127.0.0.1`), which gathers a loopback host
/// candidate so two peers sharing one host connect. Demo and test hosts use the
/// `configure` hook to opt into loopback ICE candidates via
/// [`WasiWebrtcCtx::set_setting_engine_hook`](crate::WasiWebrtcCtx::set_setting_engine_hook).
pub async fn new_peer_connection(
    configure: impl FnOnce(&mut SettingEngine),
    handler: Arc<dyn PeerConnectionEventHandler>,
) -> Result<Arc<dyn PeerConnection>> {
    new_peer_connection_with(configure, crate::WebrtcIceConfig::default(), handler).await
}

/// Like [`new_peer_connection`] but with an explicit [`WebrtcIceConfig`](crate::WebrtcIceConfig)
/// controlling the UDP bind addresses, STUN/TURN servers, and ICE transport
/// policy. A default config reproduces [`new_peer_connection`]'s built-in
/// loopback behavior; the conformance ICE lab (see `conformance/PLAN.md` Phase 5)
/// overrides it per scenario to exercise host, server-reflexive, and relay
/// candidate paths.
pub async fn new_peer_connection_with(
    configure: impl FnOnce(&mut SettingEngine),
    ice: crate::WebrtcIceConfig,
    handler: Arc<dyn PeerConnectionEventHandler>,
) -> Result<Arc<dyn PeerConnection>> {
    let mut setting = SettingEngine::default();
    configure(&mut setting);
    let runtime = default_runtime().ok_or_else(|| anyhow!("no async runtime found"))?;

    // Bind the scenario-specified interface addresses, or the crate default.
    let udp_addrs: Vec<String> = if ice.udp_addrs.is_empty() {
        vec!["127.0.0.1:0".to_string()]
    } else {
        ice.udp_addrs.clone()
    };

    // Assemble the RTCConfiguration from the scenario's STUN/TURN servers and
    // transport policy. An all-default config yields an empty builder, matching
    // the previous `RTCConfigurationBuilder::new().build()`.
    let mut config = RTCConfigurationBuilder::new();
    if !ice.ice_servers.is_empty() {
        config = config.with_ice_servers(
            ice.ice_servers
                .iter()
                .map(|server| RTCIceServer {
                    urls: server.urls.clone(),
                    username: server.username.clone(),
                    credential: server.credential.clone(),
                })
                .collect(),
        );
    }
    if ice.relay_only {
        config = config.with_ice_transport_policy(RTCIceTransportPolicy::Relay);
    }

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config.build())
        .with_setting_engine(setting)
        .with_handler(handler)
        .with_runtime(runtime)
        .with_udp_addrs(udp_addrs)
        .build()
        .await?;
    Ok(Arc::new(pc))
}

/// A [`PeerConnectionEventHandler`] built from optional callback senders.
///
/// The `webrtc` 0.20 builder takes one handler at build time; this type lets the
/// crate and its demo/test hosts assemble a handler from just the callbacks they
/// need without each defining a bespoke trait impl.
#[allow(clippy::type_complexity)]
#[derive(Default)]
pub struct CallbackHandler {
    on_ice_candidate:
        Option<Box<dyn Fn(webrtc::peer_connection::RTCPeerConnectionIceEvent) + Send + Sync>>,
    on_gathering_complete: Option<Box<dyn Fn() + Send + Sync>>,
    on_data_channel: Option<Box<dyn Fn(Arc<dyn WebrtcDataChannel>) + Send + Sync>>,
    on_connection_state:
        Option<Box<dyn Fn(webrtc::peer_connection::RTCPeerConnectionState) + Send + Sync>>,
    on_ice_connection_state:
        Option<Box<dyn Fn(webrtc::peer_connection::RTCIceConnectionState) + Send + Sync>>,
}

impl CallbackHandler {
    /// A handler with no callbacks registered.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a callback for each locally gathered ICE candidate.
    pub fn on_ice_candidate(
        mut self,
        f: impl Fn(webrtc::peer_connection::RTCPeerConnectionIceEvent) + Send + Sync + 'static,
    ) -> Self {
        self.on_ice_candidate = Some(Box::new(f));
        self
    }

    /// Register a callback fired once ICE gathering reaches `complete`.
    pub fn on_gathering_complete(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_gathering_complete = Some(Box::new(f));
        self
    }

    /// Register a callback for each data channel opened by the remote peer.
    pub fn on_data_channel(
        mut self,
        f: impl Fn(Arc<dyn WebrtcDataChannel>) + Send + Sync + 'static,
    ) -> Self {
        self.on_data_channel = Some(Box::new(f));
        self
    }

    /// Register a callback for peer-connection state transitions.
    pub fn on_connection_state(
        mut self,
        f: impl Fn(webrtc::peer_connection::RTCPeerConnectionState) + Send + Sync + 'static,
    ) -> Self {
        self.on_connection_state = Some(Box::new(f));
        self
    }

    /// Register a callback for ICE-connection state transitions.
    pub fn on_ice_connection_state(
        mut self,
        f: impl Fn(webrtc::peer_connection::RTCIceConnectionState) + Send + Sync + 'static,
    ) -> Self {
        self.on_ice_connection_state = Some(Box::new(f));
        self
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for CallbackHandler {
    async fn on_ice_candidate(&self, event: webrtc::peer_connection::RTCPeerConnectionIceEvent) {
        if let Some(f) = &self.on_ice_candidate {
            f(event);
        }
    }

    async fn on_ice_gathering_state_change(
        &self,
        state: webrtc::peer_connection::RTCIceGatheringState,
    ) {
        if state == webrtc::peer_connection::RTCIceGatheringState::Complete {
            if let Some(f) = &self.on_gathering_complete {
                f();
            }
        }
    }

    async fn on_data_channel(&self, data_channel: Arc<dyn WebrtcDataChannel>) {
        if let Some(f) = &self.on_data_channel {
            f(data_channel);
        }
    }

    async fn on_connection_state_change(
        &self,
        state: webrtc::peer_connection::RTCPeerConnectionState,
    ) {
        if let Some(f) = &self.on_connection_state {
            f(state);
        }
    }

    async fn on_ice_connection_state_change(
        &self,
        state: webrtc::peer_connection::RTCIceConnectionState,
    ) {
        if let Some(f) = &self.on_ice_connection_state {
            f(state);
        }
    }
}
