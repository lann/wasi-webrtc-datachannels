//! A sans-I/O WebRTC data-channel peer built on the wasm-capable
//! [`lann/rtc`](https://github.com/lann/rtc/tree/wasi) fork, with a native UDP
//! reference driver.
//!
//! This crate is the third stack alongside the `wasmtime-impl` (webrtc-rs) and
//! `jco-impl` (browser) hosts: instead of the fully async `webrtc-rs` engine, it
//! drives the *sans-I/O* `rtc` stack, where protocol logic is separated from
//! I/O. That separation is what lets the same peer run in a wasm guest over
//! `wasi:sockets` — the direction the `rtc` `wasi` fork unblocks.
//!
//! Two layers:
//!
//! - [`SansIoPeer`] — the runtime-agnostic core: signaling primitives plus the
//!   six sans-I/O stepping calls. It performs no I/O.
//! - [`NativePeer`] — a Tokio [`UdpSocket`](tokio::net::UdpSocket) driver that
//!   runs the event loop natively (the `native` feature, on by default).
//! - `GuestPeer` — a WASIp3 `wasi:sockets`/timer driver that runs the same core
//!   inside a wasm component (the `guest` feature). This is the guest driver
//!   `AGENTS.md` calls the natural next step.
//!
//! The [`interop`](../../wasip3_webrtc_datachannels/tests) test connects a
//! `webrtc-rs` offerer to this crate's answerer and round-trips messages over a
//! real DTLS + SCTP data channel.

mod peer;
#[cfg(feature = "native")]
mod native;
#[cfg(feature = "guest")]
mod guest;

#[cfg(feature = "native")]
pub use native::{Answered, InboundMessage, NativePeer};
#[cfg(feature = "guest")]
pub use guest::GuestPeer;
pub use peer::{PeerEvent, SansIoPeer, Transmit};
