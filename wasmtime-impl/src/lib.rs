//! Reusable Wasmtime host implementation of the `wasi:webrtc-data-channels`
//! interfaces, backed by the pure-Rust
//! [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) stack.
//!
//! This crate factors the reusable part of the Wasmtime WebRTC host out of the
//! demo binaries so any host can satisfy the `wasi:webrtc-data-channels` imports
//! with one call to [`add_to_linker`]. It is a wasip3 (component-model async)
//! implementation modeled after [`wasmtime_wasi_http::p3`]: a host embeds a
//! [`WasiWebrtcCtx`] in its store state, implements [`WasiWebrtcView`] to expose
//! it alongside the store's [`ResourceTable`], and calls [`add_to_linker`] to
//! satisfy the `types` and `data-channels` imports with a real WebRTC/SCTP data
//! channel.
//!
//! [`wasmtime_wasi_http::p3`]: https://docs.rs/wasmtime-wasi-http

pub mod bindings;
mod data_channel;
mod host;
mod pipe;

pub use data_channel::{build_echo, new_peer_connection, send_message, DataChannel};
pub use pipe::{PipeConsumer, PipeProducer};

use std::sync::Arc;

use wasmtime::component::{HasData, Linker, ResourceTable};
use webrtc::api::setting_engine::SettingEngine;

/// A hook run against a fresh [`SettingEngine`] before each peer connection is
/// created. See [`WasiWebrtcCtx::set_setting_engine_hook`].
pub type SettingEngineHook = Arc<dyn Fn(&mut SettingEngine) + Send + Sync>;

/// Configuration and per-store state for the WebRTC data-channel host.
///
/// This is intentionally minimal for the spike (mirroring `wasmtime_wasi_http`'s
/// `WasiHttpCtx`); it exists so hosts have a stable place to grow configuration
/// without changing the [`WasiWebrtcView`] shape.
///
/// The only knob so far is the [`SettingEngine`] hook (see
/// [`set_setting_engine_hook`](Self::set_setting_engine_hook)), the analogue of
/// wasmtime-wasi-http's `WasiHttpHooks`: the crate itself hardcodes no
/// environment-driven ICE behavior, leaving loopback and similar tweaks to
/// demo/test hosts.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct WasiWebrtcCtx {
    setting_engine_hook: Option<SettingEngineHook>,
}

impl std::fmt::Debug for WasiWebrtcCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasiWebrtcCtx")
            .field(
                "setting_engine_hook",
                &self.setting_engine_hook.as_ref().map(|_| "<hook>"),
            )
            .finish()
    }
}

impl WasiWebrtcCtx {
    /// Create a new, default context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook run against a fresh [`SettingEngine`] before each peer
    /// connection this context creates.
    ///
    /// This is the customization point for `webrtc-rs` behavior the crate does
    /// not opt into itself (mirroring wasmtime-wasi-http's `WasiHttpHooks`). For
    /// example, two peers sharing one host may only reach each other over
    /// loopback, so a demo/test host can enable loopback ICE candidates:
    ///
    /// ```
    /// # use wasmtime_wasi_webrtc_datachannels::WasiWebrtcCtx;
    /// let mut ctx = WasiWebrtcCtx::new();
    /// ctx.set_setting_engine_hook(|engine| {
    ///     engine.set_include_loopback_candidate(true);
    /// });
    /// ```
    pub fn set_setting_engine_hook(
        &mut self,
        hook: impl Fn(&mut SettingEngine) + Send + Sync + 'static,
    ) {
        self.setting_engine_hook = Some(Arc::new(hook));
    }

    /// The registered [`SettingEngine`] hook, if any, cheaply cloned so callers
    /// can apply it without holding a borrow of the context.
    pub fn setting_engine_hook(&self) -> Option<SettingEngineHook> {
        self.setting_engine_hook.clone()
    }

    /// Apply the registered hook (if any) to `engine`.
    pub fn configure_setting_engine(&self, engine: &mut SettingEngine) {
        if let Some(hook) = &self.setting_engine_hook {
            hook(engine);
        }
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

/// Add the `wasi:webrtc-data-channels` interfaces implemented by this crate
/// (`types` and `data-channels`) to the provided [`Linker`].
///
/// The store's data type `T` must implement [`WasiWebrtcView`]. The engine's
/// [`Config`](wasmtime::Config) must have `wasm_component_model_async` enabled,
/// since the `send`/`receive` methods use the component-model async ABI.
///
/// The `manual-signaling` interface is **not** wired here: it is a demo-only
/// surface implemented by the demo hosts on top of [`new_peer_connection`] and
/// [`DataChannel`].
///
/// # Example
///
/// ```no_run
/// use wasmtime::component::{Linker, ResourceTable};
/// use wasmtime::{Engine, Result};
/// use wasmtime_wasi_webrtc_datachannels::{
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
    Ok(())
}
