//! `cli-signaling` host: runs the manual-signaling CLI guest under Wasmtime.
//!
//! It provisions three things the guest needs and wires them onto one
//! `Linker`/`Store`:
//!
//!   * `wasi:cli@0.3` (async run + stdio) via `wasmtime_wasi::p3`, so the guest
//!     can prompt the user over stdout and read pasted blobs from stdin,
//!   * `wasi:*@0.2` via `wasmtime_wasi::p2`, which the guest's Rust `std` still
//!     lowers to, and
//!   * `wasi:webrtc-data-channels/manual-signaling` backed by `webrtc-rs`
//!     ([`ManualPeer`]), so the offer/answer exchange drives a real connection.
//!
//! Usage: `cli-signaling <component.wasm> [offerer|answerer]`.

use futures::channel::mpsc;
use futures::StreamExt;
use wasmtime::component::{
    Accessor, Component, HasData, Linker, Resource, ResourceTable, StreamReader,
};
use wasmtime::{Config, Engine, Result, Store};
use wasmtime_wasi::p3::bindings::Command;
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use wasmtime_webrtc_host::manual::{self, ManualPeer};
use wasmtime_webrtc_host::pipe::{PipeConsumer, PipeProducer};
use wasmtime_webrtc_host::EchoDataChannel;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../components/cli-signaling/wit",
        world: "manual-signaling-host",
        imports: {
            default: async | store | trappable,
            // A resource `constructor` cannot use the async ABI, so keep it
            // synchronous (the guest imports it as a plain sync function).
            "wasi:webrtc-data-channels/manual-signaling@0.1.0.[constructor]peer-connection": trappable,
        },
        with: {
            "wasi:webrtc-data-channels/data-channels.data-channel": wasmtime_webrtc_host::EchoDataChannel,
            "wasi:webrtc-data-channels/manual-signaling.peer-connection": wasmtime_webrtc_host::manual::ManualPeer,
        },
    });
}

use bindings::wasi::webrtc_data_channels::types::{DataChannelOptions, Error};

struct Ctx {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl HasData for Ctx {
    type Data<'a> = &'a mut Self;
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl bindings::wasi::webrtc_data_channels::types::Host for Ctx {}

// --- manual-signaling peer connection --------------------------------------

impl bindings::wasi::webrtc_data_channels::manual_signaling::Host for Ctx {}

impl bindings::wasi::webrtc_data_channels::manual_signaling::HostPeerConnection for Ctx {
    fn new(&mut self) -> Result<Resource<ManualPeer>> {
        Ok(self.table.push(ManualPeer::new())?)
    }
}

impl<T> bindings::wasi::webrtc_data_channels::manual_signaling::HostPeerConnectionWithStore<T>
    for Ctx
{
    async fn create_offer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        options: DataChannelOptions,
    ) -> Result<std::result::Result<String, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        Ok(peer
            .create_offer(&options.label, options.ordered, options.max_retransmits)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn accept_answer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        answer: String,
    ) -> Result<std::result::Result<(), Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        Ok(peer
            .accept_answer(answer)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn create_answer(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
        offer: String,
    ) -> Result<std::result::Result<String, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        Ok(peer
            .create_answer(offer)
            .await
            .map_err(|err| Error::Other(err.to_string())))
    }

    async fn connect(
        accessor: &Accessor<T, Self>,
        self_: Resource<ManualPeer>,
    ) -> Result<std::result::Result<Resource<EchoDataChannel>, Error>> {
        let peer = accessor.with(|mut access| clone_peer(access.get(), &self_))?;
        match peer.connect().await {
            Ok(channel) => {
                let resource = accessor.with(|mut access| access.get().table.push(channel))?;
                Ok(Ok(resource))
            }
            Err(err) => Ok(Err(Error::Other(err.to_string()))),
        }
    }

    async fn close(_accessor: &Accessor<T, Self>, _self_: Resource<ManualPeer>) -> Result<()> {
        // The peer connection is torn down when the resource is dropped.
        Ok(())
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<ManualPeer>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

/// Clone the cheaply-`Arc`-backed [`ManualPeer`] out of the table so its async
/// methods can run without holding the store borrow across `.await`.
fn clone_peer(ctx: &mut Ctx, self_: &Resource<ManualPeer>) -> Result<ManualPeer> {
    Ok(ctx.table.get(self_)?.clone())
}

// --- data channel (shared shape with the echo host) ------------------------

impl bindings::wasi::webrtc_data_channels::data_channels::Host for Ctx {}

impl bindings::wasi::webrtc_data_channels::data_channels::HostDataChannel for Ctx {}

impl<T> bindings::wasi::webrtc_data_channels::data_channels::HostDataChannelWithStore<T> for Ctx {
    async fn label(
        accessor: &Accessor<T, Self>,
        self_: Resource<EchoDataChannel>,
    ) -> Result<String> {
        accessor.with(|mut access| Ok(access.get().table.get(&self_)?.label()))
    }

    async fn send(
        accessor: &Accessor<T, Self>,
        self_: Resource<EchoDataChannel>,
        messages: StreamReader<Vec<u8>>,
    ) -> Result<std::result::Result<(), Error>> {
        let channel = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.channel())
        })?;

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        accessor.with(move |access| messages.pipe(access, PipeConsumer::new(tx)))?;

        while let Some(message) = rx.next().await {
            if let Err(err) = manual::send_message(&channel, message).await {
                return Ok(Err(Error::Other(err.to_string())));
            }
        }
        Ok(Ok(()))
    }

    async fn receive(
        accessor: &Accessor<T, Self>,
        self_: Resource<EchoDataChannel>,
    ) -> Result<StreamReader<Vec<u8>>> {
        let incoming = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.take_incoming())
        })?;
        let incoming = incoming
            .ok_or_else(|| wasmtime::Error::msg("receive() may only be called once per channel"))?;
        accessor.with(|access| StreamReader::new(access, PipeProducer::new(incoming)))
    }

    async fn drop(accessor: &Accessor<T, Self>, rep: Resource<EchoDataChannel>) -> Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = env_logger::try_init();
    let mut args = std::env::args().skip(1);
    let path = args.next().ok_or_else(|| {
        wasmtime::Error::msg("usage: cli-signaling <component.wasm> [offerer|answerer]")
    })?;
    // Remaining args are forwarded to the guest (e.g. the role).
    let guest_args: Vec<String> = std::iter::once("cli-signaling".to_string())
        .chain(args)
        .collect();

    let engine = engine()?;
    let component = Component::from_file(&engine, &path)?;

    let mut linker: Linker<Ctx> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    bindings::ManualSignalingHost::add_to_linker::<_, Ctx>(&mut linker, |c| c)?;

    let mut wasi = WasiCtx::builder();
    wasi.inherit_stdio().inherit_env().args(&guest_args);
    let mut store = Store::new(
        &engine,
        Ctx {
            wasi: wasi.build(),
            table: ResourceTable::new(),
        },
    );

    let command = Command::instantiate_async(&mut store, &component, &linker).await?;
    let result = store
        .run_concurrent(async move |store| command.wasi_cli_run().call_run(store).await)
        .await??;

    match result {
        Ok(()) => Ok(()),
        Err(()) => Err(wasmtime::Error::msg("guest signalled failure")),
    }
}
