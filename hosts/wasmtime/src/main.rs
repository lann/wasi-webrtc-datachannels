//! Wasmtime host for the `wasi:webrtc-data-channels` spike, backed by the
//! pure-Rust `webrtc-rs` stack.
//!
//! It is the non-browser counterpart to the Node host: it loads the same
//! `echo-demo` component, satisfies the `wasi:webrtc-data-channels` imports with
//! a real WebRTC/SCTP data channel (see [`webrtc`]), and invokes the component's
//! exported async `run`. The guest's outbound/inbound `stream<list<u8>>`s are
//! bridged to `webrtc-rs` via the [`pipe`] adapters.

use futures::channel::mpsc;
use futures::StreamExt;
use wasmtime::component::HasData;
use wasmtime::component::{Accessor, Component, Linker, Resource, ResourceTable, StreamReader};
use wasmtime::{Config, Engine, Result, Store};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../components/echo-demo/wit",
        world: "webrtc-echo-demo",
        imports: {
            default: async | store | trappable,
        },
        exports: {
            default: async,
        },
        with: {
            "wasi:webrtc-data-channels/data-channels.data-channel": wasmtime_webrtc_host::EchoDataChannel,
        },
    });
}

use bindings::wasi::webrtc_data_channels::types::{DataChannelOptions, Error};
use wasmtime_webrtc_host::pipe::{PipeConsumer, PipeProducer};
use wasmtime_webrtc_host::{webrtc, EchoDataChannel};

struct Ctx {
    table: ResourceTable,
}

impl HasData for Ctx {
    type Data<'a> = &'a mut Self;
}

impl bindings::wasi::webrtc_data_channels::types::Host for Ctx {}

impl bindings::demo::webrtc_echo::connect::Host for Ctx {}

impl<T> bindings::demo::webrtc_echo::connect::HostWithStore<T> for Ctx {
    async fn open_echo(
        accessor: &Accessor<T, Self>,
        options: DataChannelOptions,
    ) -> Result<std::result::Result<Resource<EchoDataChannel>, Error>> {
        let echo = match webrtc::build_echo(&options.label, options.ordered, options.max_retransmits)
            .await
        {
            Ok(echo) => echo,
            Err(err) => return Ok(Err(Error::Other(err.to_string()))),
        };
        let resource = accessor.with(|mut access| access.get().table.push(echo))?;
        Ok(Ok(resource))
    }
}

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
        let channel =
            accessor.with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.channel()))?;

        // Drain the guest's outbound stream into an mpsc sink, then forward each
        // message to the WebRTC data channel, awaiting the transport.
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        accessor.with(move |access| messages.pipe(access, PipeConsumer::new(tx)))?;

        while let Some(message) = rx.next().await {
            if let Err(err) = webrtc::send_message(&channel, message).await {
                return Ok(Err(Error::Other(err.to_string())));
            }
        }
        Ok(Ok(()))
    }

    async fn receive(
        accessor: &Accessor<T, Self>,
        self_: Resource<EchoDataChannel>,
    ) -> Result<StreamReader<Vec<u8>>> {
        let incoming = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.take_incoming()))?;
        let incoming =
            incoming.ok_or_else(|| wasmtime::Error::msg("receive() may only be called once per channel"))?;
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
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../../components/echo-demo/build/echo-demo.component.wasm".to_string());
    let message_count: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000);
    let message_size: u32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(4096);

    let engine = engine()?;
    let component = Component::from_file(&engine, &path)?;
    let mut linker: Linker<Ctx> = Linker::new(&engine);
    bindings::WebrtcEchoDemo::add_to_linker::<_, Ctx>(&mut linker, |c| c)?;

    let mut store = Store::new(&engine, Ctx { table: ResourceTable::new() });
    let demo = bindings::WebrtcEchoDemo::instantiate_async(&mut store, &component, &linker).await?;

    let started = std::time::Instant::now();
    let stats = store
        .run_concurrent(async move |accessor: &Accessor<Ctx>| {
            demo.demo_webrtc_echo_demo()
                .call_run(
                    accessor,
                    bindings::exports::demo::webrtc_echo::demo::DemoConfig {
                        message_count,
                        message_size,
                    },
                )
                .await
        })
        .await??;

    let elapsed = started.elapsed();
    match stats {
        Ok(stats) => {
            let mib = stats.bytes_echoed as f64 / (1024.0 * 1024.0);
            println!("echo-demo (Wasmtime / webrtc-rs host) result:");
            println!("  messages sent:     {}", stats.messages_sent);
            println!("  messages received: {}", stats.messages_received);
            println!("  bytes echoed:      {}", stats.bytes_echoed);
            println!(
                "  elapsed:           {:.1} ms  (~{:.1} MiB/s round-trip)",
                elapsed.as_secs_f64() * 1000.0,
                mib / elapsed.as_secs_f64()
            );
            if stats.messages_received != message_count {
                return Err(wasmtime::Error::msg(format!(
                    "expected {message_count} messages, got {}",
                    stats.messages_received
                )));
            }
            println!("\nOK: every message round-tripped through the WebRTC data channel.");
        }
        Err(err) => return Err(wasmtime::Error::msg(format!("demo returned error: {err:?}"))),
    }

    Ok(())
}
