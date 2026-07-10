//! Shared host plumbing for the Wasmtime WebRTC hosts.
//!
//! Two binaries build on this library:
//!
//! * `main` (the default `echo` binary) — the original streaming echo demo.
//! * `cli-signaling` — the manual-signaling CLI host, which pairs a real
//!   `webrtc-rs` peer connection with `wasi:cli@0.3` stdio so the guest can
//!   walk a user through a copy/paste offer/answer exchange.

pub mod manual;
pub mod pipe;
pub mod webrtc;

pub use webrtc::EchoDataChannel;
