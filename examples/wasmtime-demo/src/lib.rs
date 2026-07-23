//! Demo-only host glue for the Wasmtime WebRTC host.
//!
//! The `lann:webrtc-datachannels` host implementation (`types`,
//! `connections`, and the stream/pipe plumbing) lives in the
//! [`wasmtime_webrtc_datachannels`] crate. The binaries in this crate
//! (`wasmtime-webrtc-host`, `cli-signaling`) are thin hosts over its
//! `add_to_linker`; nothing demo-only is layered on top anymore.
