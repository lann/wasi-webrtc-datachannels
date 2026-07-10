//! Integration test for `wasmtime-wasi-webrtc-datachannels`.
//!
//! It builds the `manual-signaling-test` guest component, instantiates it under
//! Wasmtime with the crate's [`add_to_linker`] providing the
//! `wasi:webrtc-data-channels` imports, and drives a full manual-signaling
//! round trip over a real `webrtc-rs` data channel. This exercises the crate's
//! `manual-signaling` (`create-offer`/`create-answer`/`accept-answer`/`connect`)
//! and `data-channels` (`label`/`send`/`receive`) host implementations.
//!
//! [`add_to_linker`]: wasmtime_wasi_webrtc_datachannels::p3::add_to_linker

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi_webrtc_datachannels::p3::{
    add_to_linker, WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView,
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "tests/manual-signaling-guest/wit",
        world: "manual-signaling-test",
        imports: {
            default: async | store | trappable,
            "wasi:webrtc-data-channels/manual-signaling@0.1.0.[constructor]peer-connection": trappable,
        },
        exports: {
            default: async,
        },
        with: {
            "wasi:webrtc-data-channels/data-channels.data-channel":
                wasmtime_wasi_webrtc_datachannels::p3::DataChannel,
            "wasi:webrtc-data-channels/manual-signaling.peer-connection":
                wasmtime_wasi_webrtc_datachannels::p3::ManualPeer,
        },
    });
}

use bindings::exports::test::webrtc_manual_signaling::runner::Report;

/// Store state: just the WebRTC context and the shared resource table.
struct Ctx {
    webrtc: WasiWebrtcCtx,
    table: ResourceTable,
}

impl wasmtime::component::HasData for Ctx {
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

fn engine() -> Engine {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Engine::new(&config).expect("engine")
}

/// Build (once per test process) the guest component and return its bytes.
fn guest_component() -> &'static [u8] {
    static COMPONENT: OnceLock<Vec<u8>> = OnceLock::new();
    COMPONENT.get_or_init(build_guest_component)
}

/// Compile the `manual-signaling-test` guest for `wasm32-unknown-unknown` and
/// encode it as a component in-process (no dependency on the `wasm-tools`
/// binary).
fn build_guest_component() -> Vec<u8> {
    let guest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/manual-signaling-guest");
    let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("manual-signaling-guest");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    let mut command = Command::new(cargo);
    command
        .current_dir(&guest_dir)
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("--target-dir")
        .arg(&target_dir);

    // The guest cross-compiles to wasm; strip env that leaks from the outer
    // `cargo test` invocation and would otherwise break the wasm build.
    for (key, _) in std::env::vars() {
        if key.starts_with("CARGO_") || key == "RUSTFLAGS" {
            command.env_remove(key);
        }
    }

    let status = command
        .status()
        .expect("failed to spawn cargo to build the test guest");
    assert!(
        status.success(),
        "building the manual-signaling-test guest failed; ensure the \
         wasm32-unknown-unknown target is installed"
    );

    let module_path = target_dir
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("manual_signaling_test_guest.wasm");
    let module = std::fs::read(&module_path)
        .unwrap_or_else(|err| panic!("reading {}: {err}", module_path.display()));

    wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&module)
        .expect("wrapping guest module as a component")
        .encode()
        .expect("encoding guest component")
}

async fn run_round_trip(count: u32, size: u32) -> anyhow::Result<Report> {
    let engine = engine();
    let component = Component::from_binary(&engine, guest_component())?;

    let mut linker: Linker<Ctx> = Linker::new(&engine);
    add_to_linker(&mut linker)?;

    let mut store = Store::new(
        &engine,
        Ctx {
            webrtc: WasiWebrtcCtx::new(),
            table: ResourceTable::new(),
        },
    );

    let instance =
        bindings::ManualSignalingTest::instantiate_async(&mut store, &component, &linker).await?;

    let result = store
        .run_concurrent(async move |accessor| {
            instance
                .test_webrtc_manual_signaling_runner()
                .call_run(accessor, count, size)
                .await
        })
        .await??;

    result.map_err(|err| anyhow::anyhow!("guest returned error: {err:?}"))
}

#[test]
fn manual_signaling_round_trip() {
    // Two peer connections share this process; allow loopback ICE candidates so
    // they can pair even when no other local address is mutually reachable.
    // SAFETY: set before any peer connection is built and the test is single
    // threaded with respect to this variable.
    unsafe {
        std::env::set_var("WEBRTC_INCLUDE_LOOPBACK", "1");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let count = 64;
    let size = 1024;
    let report = runtime
        .block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(60), run_round_trip(count, size))
                .await
        })
        .expect("manual-signaling round trip timed out")
        .expect("manual-signaling round trip failed");

    assert_eq!(report.label, "manual-signaling-test");
    assert_eq!(report.sent, count);
    assert_eq!(
        report.received, count,
        "every message should round-trip through the data channel"
    );
    assert_eq!(report.bytes, u64::from(count) * u64::from(size));
}
