//! A wasm guest driver: [`GuestPeer`].
//!
//! This backs the sans-I/O [`SansIoPeer`] with a WASIp3 `wasi:sockets` UDP
//! socket and `wasi:clocks` timer, running the event loop *inside a wasm
//! component* rather than on a native Tokio runtime. It is the counterpart of
//! [`crate::native`]'s `NativePeer`: same core, different I/O. Because
//! [`SansIoPeer`] does no I/O of its own, the two drivers share it unchanged —
//! this is exactly the guest driver `AGENTS.md` calls the natural next step.
//!
//! The component-model async model is single-threaded and cooperative (there is
//! no `spawn` onto other OS threads), so [`GuestPeer`] exposes a *pump* rather
//! than a background task: the caller repeatedly [`flush`](GuestPeer::flush)es
//! outbound datagrams, [`drain_events`](GuestPeer::drain_events) to observe
//! changes, and [`wait`](GuestPeer::wait)s on the earliest of a timer or an
//! inbound datagram. A caller can drive several peers this way by interleaving
//! their pumps under `futures::join!`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Instant;

use anyhow::{anyhow, Result};
use futures::future::{select, Either};

use wasip3::clocks::monotonic_clock;
use wasip3::sockets::types::{
    ErrorCode, IpAddressFamily, IpSocketAddress, Ipv4SocketAddress, Ipv6SocketAddress, UdpSocket,
};

use crate::peer::{PeerEvent, SansIoPeer};

/// The safety-net wake interval. Mirrors [`crate::native`]'s cap: a short bound
/// on how long the pump can sleep when [`SansIoPeer::poll_timeout`] returns
/// `None`, so retransmit and keep-alive timers still fire promptly.
const MAX_WAIT_NANOS: u64 = 50_000_000;

/// A sans-I/O peer bound to a WASIp3 UDP socket, driven by the caller's pump.
pub struct GuestPeer {
    peer: SansIoPeer,
    socket: UdpSocket,
    local: SocketAddr,
}

impl GuestPeer {
    /// Bind a UDP socket on `local_ip` (an ephemeral port) and pair it with
    /// `peer`. The bound address is available from
    /// [`local_addr`](Self::local_addr) so the caller can hand it to
    /// [`SansIoPeer::add_local_host_candidate`].
    pub fn bind(peer: SansIoPeer, local_ip: IpAddr) -> Result<Self> {
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
        Ok(Self {
            peer,
            socket,
            local,
        })
    }

    /// The socket's bound local address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The wrapped sans-I/O peer, for signaling (offer/answer/candidates) and
    /// message sends.
    pub fn peer(&mut self) -> &mut SansIoPeer {
        &mut self.peer
    }

    /// Send every currently queued outbound datagram.
    pub async fn flush(&mut self) {
        while let Some(transmit) = self.peer.poll_transmit() {
            let _ = self
                .socket
                .send(transmit.payload, Some(to_wasi_addr(transmit.destination)))
                .await;
        }
    }

    /// Drain all currently available peer events (connection-state changes,
    /// opened data channels, and inbound messages).
    pub fn drain_events(&mut self) -> Vec<PeerEvent> {
        self.peer.drain_events()
    }

    /// Park until the earliest of the peer's next timer deadline or an inbound
    /// datagram, then feed that into the peer. Call in a loop, flushing and
    /// draining around it.
    pub async fn wait(&mut self) {
        // Destructure so the receive future can borrow `socket` while the
        // branch handlers borrow `peer` — disjoint fields, no conflict.
        let Self {
            peer,
            socket,
            local,
        } = self;

        let delay = peer
            .poll_timeout()
            .map(|deadline| {
                let now = Instant::now();
                deadline
                    .saturating_duration_since(now)
                    .as_nanos()
                    .min(u128::from(MAX_WAIT_NANOS)) as u64
            })
            .unwrap_or(MAX_WAIT_NANOS);

        let timer = std::pin::pin!(monotonic_clock::wait_for(delay));
        let recv = std::pin::pin!(socket.receive());

        match select(timer, recv).await {
            Either::Left(_) => peer.handle_timeout(Instant::now()),
            Either::Right((Ok((data, from)), _)) => {
                peer.handle_input(&data, from_wasi_addr(from), *local, Instant::now());
            }
            // A receive error just means no datagram to feed this round; the
            // pump loops and tries again.
            Either::Right((Err(_), _)) => {}
        }
    }
}

/// Convert a WASIp3 socket error into an `anyhow` error carrying `what`.
fn net_err(what: &'static str) -> impl Fn(ErrorCode) -> anyhow::Error {
    move |code| anyhow!("{what}: {code}")
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
            SocketAddr::new(
                IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h)),
                v6.port,
            )
        }
    }
}
