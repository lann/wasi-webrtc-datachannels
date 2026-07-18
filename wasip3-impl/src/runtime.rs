//! The in-guest driver that runs a [`SansIoPeer`] over WASIp3 `wasi:sockets`
//! UDP and `wasi:clocks` timers.
//!
//! A [`Runtime`] owns one peer connection's socket and sans-I/O core. Because
//! the component-model async model is single-threaded and cooperative (no
//! cross-thread `spawn`), the event loop runs as a detached task started with
//! [`wit_bindgen::spawn`]: [`Runtime::pump`] repeatedly flushes queued
//! datagrams, drains the core's events into the shared queues that back the
//! exported resources, and parks on the earliest of a timer or an inbound
//! datagram. The exported `data-channel` / `peer-connection` methods observe
//! that shared state and wake the pump through [`futures`] channels.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::rc::Rc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use futures::channel::mpsc;
use futures::future::{select, Either};
use futures::StreamExt;

use rtc::data_channel::RTCDataChannelId;

use crate::peer::{PeerEvent, SansIoPeer};

use crate::wasi::clocks::monotonic_clock;
use crate::wasi::sockets::types::{
    ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress, UdpSocket,
};

/// The safety-net wake interval, so the pump re-checks timers even when the
/// stack reports no deadline. Mirrors the reference driver's 50ms cap: a short
/// bound on how long the pump can sleep so retransmit and keep-alive timers
/// still fire promptly.
const MAX_WAIT_NANOS: u64 = 50_000_000;

/// A received data-channel message, queued for the owning `data-channel`.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Whether the payload was sent as text (UTF-8) rather than binary.
    pub text: bool,
    /// The message payload.
    pub data: Vec<u8>,
}

/// A data channel observed by the peer connection: its id plus the label and an
/// inbound-message queue that the exported `data-channel` drains.
pub struct Channel {
    /// The channel's sans-I/O id.
    pub id: RTCDataChannelId,
    /// The negotiated channel label.
    pub label: String,
    /// Messages received on this channel, oldest first.
    pub inbox: VecDeque<InboundMessage>,
    /// True once the channel (or its connection) has closed.
    pub closed: bool,
}

impl Channel {
    fn new(id: RTCDataChannelId, label: String) -> Self {
        Self {
            id,
            label,
            inbox: VecDeque::new(),
            closed: false,
        }
    }
}

/// The connection-level state shared between the pump and the exported
/// resources.
pub struct Shared {
    /// The sans-I/O core.
    pub peer: SansIoPeer,
    /// Every channel seen so far, in open order.
    pub channels: Vec<Channel>,
    /// Whether the connection reached `connected`.
    pub connected: bool,
    /// Whether the connection failed.
    pub failed: bool,
    /// Whether the connection closed.
    pub closed: bool,
}

impl Shared {
    /// Find a channel by id.
    pub fn channel_mut(&mut self, id: RTCDataChannelId) -> Option<&mut Channel> {
        self.channels.iter_mut().find(|c| c.id == id)
    }
}

/// One peer connection's driver: the shared state plus the socket the pump uses.
pub struct Runtime {
    /// The connection state, shared with the exported resources and the pump.
    pub shared: Rc<RefCell<Shared>>,
    socket: Rc<UdpSocket>,
    local: SocketAddr,
    /// Sends a unit each time an exported method changes the core (a queued
    /// send, a new channel, a close), so the pump re-flushes promptly.
    wake_tx: mpsc::UnboundedSender<()>,
}

impl Runtime {
    /// Bind a UDP socket on loopback-capable `local_ip` (an ephemeral port),
    /// pair it with `peer`, and supply the resulting host candidate to the
    /// core. Returns the runtime and a wake receiver for [`pump`](Self::pump).
    pub fn bind(
        mut peer: SansIoPeer,
        local_ip: IpAddr,
    ) -> Result<(Self, mpsc::UnboundedReceiver<()>, String)> {
        let family = match local_ip {
            IpAddr::V4(_) => IpAddressFamily::Ipv4,
            IpAddr::V6(_) => IpAddressFamily::Ipv6,
        };
        let socket = UdpSocket::create(family).map_err(net_err("create UDP socket"))?;
        socket
            .bind(to_wasi_addr(SocketAddr::new(local_ip, 0)))
            .map_err(net_err("bind UDP socket"))?;
        let local = from_wasi_addr(
            socket
                .get_local_address()
                .map_err(net_err("read bound address"))?,
        );

        let candidate = peer.add_local_host_candidate(local)?;

        let (wake_tx, wake_rx) = mpsc::unbounded();
        let shared = Rc::new(RefCell::new(Shared {
            peer,
            channels: Vec::new(),
            connected: false,
            failed: false,
            closed: false,
        }));

        Ok((
            Self {
                shared,
                socket: Rc::new(socket),
                local,
                wake_tx,
            },
            wake_rx,
            candidate,
        ))
    }

    /// A cheap handle the exported resources use to nudge the pump after they
    /// mutate the core.
    pub fn waker(&self) -> mpsc::UnboundedSender<()> {
        self.wake_tx.clone()
    }

    /// The shared state handle.
    pub fn shared(&self) -> Rc<RefCell<Shared>> {
        self.shared.clone()
    }

    /// Run the event loop until the connection closes or fails. Started once per
    /// peer connection via [`wit_bindgen::spawn`].
    pub async fn pump(self, mut wake_rx: mpsc::UnboundedReceiver<()>) {
        let shared = self.shared;
        let socket = self.socket;
        let local = self.local;

        loop {
            // Flush + drain while holding the borrow only between awaits.
            flush(&shared, &socket).await;
            {
                let mut s = shared.borrow_mut();
                for event in s.peer.drain_events() {
                    apply_event(&mut s, event);
                }
                if s.closed || s.failed {
                    return;
                }
            }
            flush(&shared, &socket).await;

            // The stack's next timer deadline (if any). Wake at that instant,
            // capped by the safety net so retransmit/keep-alive timers fire even
            // when the stack reports no deadline.
            let deadline = shared.borrow_mut().peer.poll_timeout();
            let now = Instant::now();
            let delay = deadline
                .map(|d| {
                    d.saturating_duration_since(now)
                        .as_nanos()
                        .min(u128::from(MAX_WAIT_NANOS)) as u64
                })
                .unwrap_or(MAX_WAIT_NANOS);

            let timer = std::pin::pin!(monotonic_clock::wait_for(delay));
            let recv = std::pin::pin!(socket.receive());
            let wake = std::pin::pin!(wake_rx.next());

            // Wait on the earliest of a timer, an inbound datagram, or a wake.
            match select(select(timer, recv), wake).await {
                Either::Left((Either::Left(_), _)) => {
                    // Feed the stack a time guaranteed to be at or past the
                    // deadline it asked for. Passing `Instant::now()` alone can
                    // be a few nanoseconds short of `deadline` (the host timer
                    // may return early / clock granularity), leaving the timer
                    // unexpired: the stack would report the same past deadline
                    // again, the pump would spin with `delay == 0`, and no
                    // retransmit would ever be produced — stalling the
                    // handshake. Using `max(now, deadline)` guarantees progress.
                    let fire_at = match deadline {
                        Some(d) => d.max(Instant::now()),
                        None => Instant::now(),
                    };
                    shared.borrow_mut().peer.handle_timeout(fire_at);
                }
                Either::Left((Either::Right((Ok((data, from)), _)), _)) => {
                    shared.borrow_mut().peer.handle_input(
                        &data,
                        from_wasi_addr(from),
                        local,
                        Instant::now(),
                    );
                }
                // A receive error just means no datagram this round; loop again.
                Either::Left((Either::Right((Err(_), _)), _)) => {}
                // A wake (an exported method mutated the core): loop to flush.
                Either::Right(_) => {}
            }
        }
    }
}

/// Fold one drained core event into the shared state.
fn apply_event(s: &mut Shared, event: PeerEvent) {
    match event {
        PeerEvent::Connected => s.connected = true,
        PeerEvent::Failed => {
            s.failed = true;
            for c in &mut s.channels {
                c.closed = true;
            }
        }
        PeerEvent::Closed => {
            s.closed = true;
            for c in &mut s.channels {
                c.closed = true;
            }
        }
        PeerEvent::ChannelOpen { id, label } => {
            if s.channel_mut(id).is_none() {
                s.channels.push(Channel::new(id, label));
            }
        }
        PeerEvent::Message { id, text, data } => {
            if let Some(channel) = s.channel_mut(id) {
                channel.inbox.push_back(InboundMessage { text, data });
            }
        }
    }
}

/// Send every currently queued outbound datagram, borrowing the core only to
/// pull each one (never across the `send` await).
async fn flush(shared: &Rc<RefCell<Shared>>, socket: &UdpSocket) {
    loop {
        let transmit = shared.borrow_mut().peer.poll_transmit();
        let Some(transmit) = transmit else { break };
        let _ = socket
            .send(transmit.payload, Some(to_wasi_addr(transmit.destination)))
            .await;
    }
}

/// Convert a WASIp3 socket error into an `anyhow` error carrying `what`.
fn net_err(what: &'static str) -> impl Fn(ErrorCode) -> anyhow::Error {
    move |code| anyhow!("{what}: {code:?}")
}

/// Convert a `std` [`SocketAddr`] into a WASIp3 [`IpSocketAddress`].
fn to_wasi_addr(addr: SocketAddr) -> IpSocketAddress {
    match addr {
        SocketAddr::V4(v4) => {
            let [a, b, c, d] = v4.ip().octets();
            IpSocketAddress::Ipv4(Ipv4SocketAddress {
                port: v4.port(),
                address: (a, b, c, d),
            })
        }
        SocketAddr::V6(v6) => {
            let [a, b, c, d, e, f, g, h] = v6.ip().segments();
            IpSocketAddress::Ipv6(Ipv6SocketAddress {
                port: v6.port(),
                flow_info: v6.flowinfo(),
                address: (a, b, c, d, e, f, g, h),
                scope_id: v6.scope_id(),
            })
        }
    }
}

/// Convert a WASIp3 [`IpSocketAddress`] into a `std` [`SocketAddr`].
fn from_wasi_addr(addr: IpSocketAddress) -> SocketAddr {
    match addr {
        IpSocketAddress::Ipv4(v4) => {
            let (a, b, c, d) = v4.address;
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(a, b, c, d)), v4.port)
        }
        IpSocketAddress::Ipv6(v6) => {
            let (a, b, c, d, e, f, g, h) = v6.address;
            SocketAddr::new(IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h)), v6.port)
        }
    }
}
