//! Wasmtime host for `lann:webrtc-datachannels`, backed by the
//! pure-Rust `webrtc-rs` stack.
//!
//! It is the non-browser counterpart to the Node host: it loads the same
//! `echo-demo` component and invokes the component's exported async `run`. The
//! `lann:webrtc-datachannels` imports (`types`, `data-channels`) are
//! satisfied by [`wasmtime_webrtc_datachannels`]; this binary only
//! implements the demo-only `connect` convenience, which wires a channel to a
//! host-provided echo endpoint via a local helper.

use std::sync::{Arc, Mutex};

use anyhow::anyhow;
use futures::channel::{mpsc, oneshot};
use wasmtime::component::{
    Accessor, Component, HasData, Linker, Resource, ResourceTable, StreamReader,
};
use wasmtime::{Config, Engine, Result, Store};
use wasmtime_webrtc_datachannels::{
    self as webrtc_host, inbound_stream, new_peer_connection, DataChannel, WasiWebrtcCtx,
    WasiWebrtcCtxView, WasiWebrtcView,
};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::data_channel::data_channel_init::RTCDataChannelInit;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../echo-demo/wit",
        world: "webrtc-echo-demo",
        imports: {
            default: async | store | trappable,
        },
        exports: {
            default: async,
        },
        with: {
            "lann:webrtc-datachannels/data-channels.data-channel":
                wasmtime_webrtc_datachannels::DataChannel,
        },
    });
}

use bindings::lann::webrtc_datachannels::types::{DataChannelOptions, Error};

struct Ctx {
    webrtc: WasiWebrtcCtx,
    table: ResourceTable,
}

impl HasData for Ctx {
    type Data<'a> = &'a mut Self;
}

impl WasiWebrtcView for Ctx {
    fn webrtc(&mut self) -> WasiWebrtcCtxView<'_> {
        WasiWebrtcCtxView {
            ctx: &mut self.webrtc,
            table: &mut self.table,
        }
    }
}

// The demo-only `connect` convenience is implemented here; the
// `data-channels`/`types` imports come from the crate's `add_to_linker`.
impl bindings::demo::webrtc_echo::connect::Host for Ctx {}

impl<T> bindings::demo::webrtc_echo::connect::HostWithStore<T> for Ctx {
    async fn open_echo(
        accessor: &Accessor<T, Self>,
        options: DataChannelOptions,
    ) -> Result<std::result::Result<(Resource<DataChannel>, StreamReader<Vec<u8>>), Error>> {
        // The two echo peers live in this one process, so apply the store's
        // `SettingEngine` hook (e.g. loopback ICE candidates) to each of them.
        let hook = accessor.with(|mut access| {
            Ok::<_, wasmtime::Error>(access.get().webrtc.setting_engine_hook())
        })?;
        let (echo, incoming) = match build_echo(
            &options.label,
            options.ordered,
            options.max_retransmits,
            |engine| {
                if let Some(hook) = &hook {
                    hook(engine);
                }
            },
        )
        .await
        {
            Ok(echo) => echo,
            Err(err) => return Ok(Err(Error::Other(err.to_string()))),
        };
        let resource = accessor.with(|mut access| access.get().table.push(echo))?;
        // Produce the inbound stream once, at construction, and hand it back
        // alongside the channel resource.
        let stream = inbound_stream(accessor, incoming)?;
        Ok(Ok((resource, stream)))
    }
}

async fn build_echo(
    label: &str,
    ordered: bool,
    max_retransmits: Option<u16>,
    configure: impl Fn(&mut SettingEngine),
) -> anyhow::Result<(DataChannel, mpsc::UnboundedReceiver<Vec<u8>>)> {
    let near = new_peer_connection(&configure).await?;
    let far = new_peer_connection(&configure).await?;

    let far_for_ice = far.clone();
    near.on_ice_candidate(Box::new(move |candidate| {
        let far = far_for_ice.clone();
        Box::pin(async move {
            if let Some(candidate) = candidate {
                if let Ok(init) = candidate.to_json() {
                    let _ = far.add_ice_candidate(init).await;
                }
            }
        })
    }));
    let near_for_ice = near.clone();
    far.on_ice_candidate(Box::new(move |candidate| {
        let near = near_for_ice.clone();
        Box::pin(async move {
            if let Some(candidate) = candidate {
                if let Ok(init) = candidate.to_json() {
                    let _ = near.add_ice_candidate(init).await;
                }
            }
        })
    }));

    far.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        Box::pin(async move {
            let echo_channel = channel.clone();
            channel.on_message(Box::new(move |message: DataChannelMessage| {
                let echo_channel = echo_channel.clone();
                Box::pin(async move {
                    let _ = echo_channel.send(&message.data).await;
                })
            }));
        })
    }));

    let init = RTCDataChannelInit {
        ordered: Some(ordered),
        max_retransmits,
        ..Default::default()
    };
    let channel = near.create_data_channel(label, Some(init)).await?;

    let (in_tx, in_rx) = mpsc::unbounded::<Vec<u8>>();
    channel.on_message(Box::new(move |message: DataChannelMessage| {
        let in_tx = in_tx.clone();
        Box::pin(async move {
            let _ = in_tx.unbounded_send(message.data.to_vec());
        })
    }));

    let (open_tx, open_rx) = oneshot::channel::<()>();
    let open_tx = Arc::new(Mutex::new(Some(open_tx)));
    channel.on_open(Box::new(move || {
        let open_tx = open_tx.clone();
        Box::pin(async move {
            if let Some(tx) = open_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
        })
    }));

    let offer = near.create_offer(None).await?;
    near.set_local_description(offer.clone()).await?;
    far.set_remote_description(offer).await?;
    let answer = far.create_answer(None).await?;
    far.set_local_description(answer.clone()).await?;
    near.set_remote_description(answer).await?;

    open_rx
        .await
        .map_err(|_| anyhow!("data channel closed before opening"))?;

    Ok((DataChannel::new(channel, vec![near, far]), in_rx))
}

fn engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config)
}

/// Build the WebRTC context, opting into loopback ICE candidates when the
/// `WEBRTC_INCLUDE_LOOPBACK` environment variable is set. `build_echo` stands up
/// both peers in this one process, so on hosts without another mutually
/// reachable address this is required for them to pair.
fn webrtc_ctx() -> WasiWebrtcCtx {
    let mut ctx = WasiWebrtcCtx::new();
    if std::env::var_os("WEBRTC_INCLUDE_LOOPBACK").is_some() {
        ctx.set_setting_engine_hook(|engine| {
            engine.set_include_loopback_candidate(true);
        });
    }
    ctx
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../echo-demo/build/echo-demo.component.wasm".to_string());
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
    // Shared `lann:webrtc-datachannels` imports.
    webrtc_host::add_to_linker(&mut linker)?;
    // Demo-only `connect` import.
    bindings::demo::webrtc_echo::connect::add_to_linker::<_, Ctx>(&mut linker, |c| c)?;

    let mut store = Store::new(
        &engine,
        Ctx {
            webrtc: webrtc_ctx(),
            table: ResourceTable::new(),
        },
    );
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
        Err(err) => {
            return Err(wasmtime::Error::msg(format!(
                "demo returned error: {err:?}"
            )))
        }
    }

    Ok(())
}
