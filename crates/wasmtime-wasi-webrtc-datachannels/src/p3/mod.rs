//! wasip3 (component-model async) host implementation of the reusable
//! `wasi:webrtc-data-channels` interfaces, backed by the pure-Rust
//! [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) stack.
//!
//! This module is modeled after [`wasmtime_wasi_http::p3`]: a host embeds a
//! [`WasiWebrtcCtx`] in its store state, implements [`WasiWebrtcView`] to expose
//! it alongside the store's [`ResourceTable`], and calls [`add_to_linker`] to
//! satisfy the `types`, `data-channels`, and `manual-signaling` imports with a
//! real WebRTC/SCTP data channel.
//!
//! [`wasmtime_wasi_http::p3`]: https://docs.rs/wasmtime-wasi-http

pub mod bindings;
mod data_channel;
mod host;
mod manual;
mod pipe;

pub use data_channel::{build_echo, new_peer_connection, send_message, DataChannel};
pub use manual::ManualPeer;
pub use pipe::{PipeConsumer, PipeProducer};

use wasmtime::component::{HasData, Linker, ResourceTable};

/// Configuration and per-store state for the WebRTC data-channel host.
///
/// This is intentionally minimal for the spike (mirroring `wasmtime_wasi_http`'s
/// `WasiHttpCtx`); it exists so hosts have a stable place to grow configuration
/// without changing the [`WasiWebrtcView`] shape. Loopback ICE candidates are
/// currently opted into with the `WEBRTC_INCLUDE_LOOPBACK` environment variable
/// (see [`new_peer_connection`]).
#[derive(Debug, Default)]
#[non_exhaustive]
pub struct WasiWebrtcCtx {}

impl WasiWebrtcCtx {
    /// Create a new, default context.
    pub fn new() -> Self {
        Self::default()
    }
}

/// A borrowed view into a host's [`WasiWebrtcCtx`] and its [`ResourceTable`].
///
/// Returned by [`WasiWebrtcView::webrtc`], this is the [`HasData::Data`] the
/// generated host bindings operate on.
pub struct WasiWebrtcCtxView<'a> {
    /// Mutable reference to the WebRTC host context.
    pub ctx: &'a mut WasiWebrtcCtx,
    /// Mutable reference to the table used to manage host resources.
    pub table: &'a mut ResourceTable,
}

/// A trait which provides access to the [`WasiWebrtcCtx`] host state.
///
/// Implement this for your store's data type so [`add_to_linker`] can wire the
/// `wasi:webrtc-data-channels` imports onto your linker.
pub trait WasiWebrtcView: Send {
    /// Return a [`WasiWebrtcCtxView`] from a mutable reference to `self`.
    fn webrtc(&mut self) -> WasiWebrtcCtxView<'_>;
}

/// The type for which this crate implements the `wasi:webrtc-data-channels`
/// interfaces. Used as the [`HasData`] marker for the generated bindings.
pub struct WasiWebrtc;

impl HasData for WasiWebrtc {
    type Data<'a> = WasiWebrtcCtxView<'a>;
}

/// Add all `wasi:webrtc-data-channels` interfaces implemented by this crate
/// (`types`, `data-channels`, and `manual-signaling`) to the provided
/// [`Linker`].
///
/// The store's data type `T` must implement [`WasiWebrtcView`]. The engine's
/// [`Config`](wasmtime::Config) must have `wasm_component_model_async` enabled,
/// since the `send`/`receive` and signaling methods use the component-model
/// async ABI.
///
/// # Example
///
/// ```no_run
/// use wasmtime::component::{Linker, ResourceTable};
/// use wasmtime::{Engine, Result};
/// use wasmtime_wasi_webrtc_datachannels::p3::{
///     add_to_linker, WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView,
/// };
///
/// struct MyState {
///     webrtc: WasiWebrtcCtx,
///     table: ResourceTable,
/// }
///
/// impl WasiWebrtcView for MyState {
///     fn webrtc(&mut self) -> WasiWebrtcCtxView<'_> {
///         WasiWebrtcCtxView {
///             ctx: &mut self.webrtc,
///             table: &mut self.table,
///         }
///     }
/// }
///
/// fn wire(linker: &mut Linker<MyState>) -> Result<()> {
///     add_to_linker(linker)
/// }
/// ```
pub fn add_to_linker<T>(linker: &mut Linker<T>) -> wasmtime::Result<()>
where
    T: WasiWebrtcView + 'static,
{
    bindings::webrtc_data_channels::types::add_to_linker::<_, WasiWebrtc>(linker, T::webrtc)?;
    bindings::webrtc_data_channels::data_channels::add_to_linker::<_, WasiWebrtc>(
        linker,
        T::webrtc,
    )?;
    bindings::webrtc_data_channels::manual_signaling::add_to_linker::<_, WasiWebrtc>(
        linker,
        T::webrtc,
    )?;
    Ok(())
}
