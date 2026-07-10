//! Reusable Wasmtime host implementation of the `wasi:webrtc-data-channels`
//! interfaces, backed by the pure-Rust
//! [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) stack.
//!
//! This crate factors the reusable part of the Wasmtime WebRTC host out of the
//! demo binaries so any host can satisfy the `wasi:webrtc-data-channels` imports
//! with one call to [`p3::add_to_linker`]. It is modeled after the
//! [`wasmtime_wasi_http::p3`] module.
//!
//! The wasip3 (component-model async) implementation lives in the [`p3`] module.
//!
//! [`wasmtime_wasi_http::p3`]: https://docs.rs/wasmtime-wasi-http

pub mod p3;
