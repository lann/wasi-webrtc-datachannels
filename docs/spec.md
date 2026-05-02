# WASI WebRTC DataChannels interface and implementations

The project includes a [WebAssembly Component
Model](https://github.com/WebAssembly/component-model) interface for a subset of
WebRTC exposing enough of RTCPeerConnection, RTCDataChannel, and associated
interfaces to enable the implementation of compatible host implementations for
browsers and non-browser environments.

## Interface

The interface is specified as WebAssembly Component Model
[WIT](https://github.com/WebAssembly/component-model/blob/main/design/mvp/WIT.md)
document(s) in the `wit/` directory. Async features of the Component Model
(primarily described
[here](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Concurrency.md))
are used.

The interface designs aim to strike a balance between matching conventions set
by WASI interfaces (such as `udp-socket` from
[`wasi:sockets@0.3`](https://github.com/WebAssembly/WASI/blob/main/proposals/sockets/wit-0.3.0-draft/types.wit))
and the [W3C WebRTC spec](https://w3c.github.io/webrtc-pc/) with design goals of
providing excellent guest language bindings while ensuring that host
implementations are feasible.

## Host Implementations

### Browser

The host implementation for browsers depends on bleeding-edge support for
Component Model async features in the [JCO
project](https://github.com/bytecodealliance/jco). Additionally, browser support
depends on the [JavaScript Promise Integration Proposal for
WebAssembly](https://github.com/WebAssembly/js-promise-integration) which may
require enabling experimental features in JCO and some runtime environments.

### Node(-compatible environments)

Support for Node.js is based on the browser implementation along with
[`node-webrtc`](https://github.com/node-webrtc/node-webrtc).

### Wasmtime

A host implementation of the `wasi:webrtc-datachannels` interface for
[Wasmtime](https://docs.rs/wasmtime/latest/wasmtime/) is in
`crates/wasmtime-wasi-webrtc-datachannels`. It uses crates from the [`webrtc-rs`
project](https://github.com/webrtc-rs/webrtc).

This implementation is used by `crates/wasi-webrtc-datachannels-runner` to
provide a basic runtime environment for executing components that use this
interface.