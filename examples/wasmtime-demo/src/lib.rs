//! Demo-only host glue for the Wasmtime WebRTC host.
//!
//! The reusable `wasi:webrtc-data-channels` host implementation (`types`,
//! `data-channels`, and the stream/pipe plumbing) lives in the
//! [`wasmtime_wasi_webrtc_datachannels`] crate. This library only carries the
//! demo-only pieces layered on top of it — currently the `manual-signaling`
//! host implementation, shared by the `cli-signaling` binary and the crate's
//! integration test.

pub mod manual;
