//! Raw `bindgen!` output for the reusable `wasi:webrtc-data-channels` package.
//!
//! Only the interfaces this crate implements are wired up (`types`,
//! `data-channels`, `manual-signaling`); see [`crate`] for the public API built
//! on top of these bindings.

#[allow(missing_docs, reason = "generated code")]
mod generated {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "imports",
        imports: {
            // The high-throughput `send`/`receive` and the async signaling
            // methods use the component-model async ABI and need `Accessor`
            // access to the store.
            default: async | store | trappable,
            // These are synchronous functions in the WIT and are imported as
            // such by guests, so they must be bound synchronously (a resource
            // `constructor` also cannot use the async ABI).
            "wasi:webrtc-data-channels/data-channels@0.1.0.[method]data-channel.label": trappable,
            "wasi:webrtc-data-channels/manual-signaling@0.1.0.[constructor]peer-connection": trappable,
            "wasi:webrtc-data-channels/manual-signaling@0.1.0.[method]peer-connection.close": trappable,
        },
        with: {
            "wasi:webrtc-data-channels/data-channels.data-channel": crate::p3::DataChannel,
            "wasi:webrtc-data-channels/manual-signaling.peer-connection": crate::p3::ManualPeer,
        },
    });
}

pub use self::generated::wasi::*;
