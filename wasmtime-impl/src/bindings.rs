//! Raw `bindgen!` output for the `wasi:webrtc-data-channels` package.
//!
//! Only the interfaces this crate implements are wired up (`types` and
//! `data-channels`); see [`crate`] for the public API built on top of these
//! bindings.

#[allow(missing_docs, reason = "generated code")]
mod generated {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "imports",
        imports: {
            // `send`/`receive`/`drop` need all three: `async` for the
            // component-model async ABI, `store` for `Accessor` access to the
            // `ResourceTable` (and the `…WithStore` traits that host the async
            // `drop`), and `trappable` so the host functions can return
            // `wasmtime::Result` and surface host errors as traps. Dropping any
            // one of them fails to compile against these host impls.
            default: async | store | trappable,
            // `data-channel.label` is a synchronous function in the WIT and is
            // imported as such by guests, so it must be bound synchronously
            // (still `trappable`, but not `async`).
            "wasi:webrtc-data-channels/data-channels@0.1.0.[method]data-channel.label": trappable,
        },
        with: {
            "wasi:webrtc-data-channels/data-channels.data-channel": crate::DataChannel,
        },
    });
}

pub use self::generated::wasi::*;
