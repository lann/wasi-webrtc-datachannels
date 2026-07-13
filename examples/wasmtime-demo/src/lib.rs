//! Demo-only host glue for the Wasmtime WebRTC host.
//!
//! The `lann:webrtc-datachannels` host implementation (`types`,
//! `connections`, and the stream/pipe plumbing) lives in the
//! [`wasmtime_webrtc_datachannels`] crate. This library only carries the
//! demo-only pieces layered on top of it — currently the `manual-signaling`
//! host implementation.

pub mod manual;
