//! Conformance adapter for the wasmtime (native `webrtc-rs`) target.
//!
//! It runs the shared conformance guest component against the wasmtime host
//! ([`wasmtime_webrtc_datachannels`], provisioned by
//! [`conformance_adapter_wasmtime`]) and emits an adapter result document the
//! conformance runner consumes. For each registered test it:
//!
//! - decides how many guest instances the test needs — a single `both` instance
//!   stands up both peers in-process (no external signaling) for the
//!   peer-connection API tests, or two instances (an `offerer` and an
//!   `answerer`) share one signaling room for the behavioral/interop tests;
//! - provisions each instance's store with the wasmtime WebRTC host (loopback
//!   ICE enabled so two same-host peers pair) and a native HTTP `mailbox` host
//!   backed by an in-process `conformance-signalingd`;
//! - drives the guest's exported `run-test` to a WIT-observable outcome and
//!   folds the per-instance results into one raw `pass`/`fail`/`skip`.
//!
//! The guest owns every assertion; the adapter only provisions, orchestrates,
//! and records.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use wasmtime::component::Component;
use wasmtime::Engine;

use conformance_adapter_common::{
    default_is_flaky, fold_two, params_for, plan_for, run_corpus, run_with_retries, write_report,
    AdapterReport, Plan, RawResult, RetryPolicy, TestOutcome, TESTS,
};
use conformance_adapter_wasmtime::{build_engine, make_config, run_instance, Role};

// ----- guest orchestration --------------------------------------------------

/// Run a two-peer test: an offerer and an answerer share `room`, driven
/// concurrently so each can consume the other's mailbox as it publishes.
async fn run_two_peer(
    engine: &Engine,
    component: &Component,
    test_id: &str,
    base_url: &str,
    room: &str,
    count: u32,
    size: u32,
) -> Result<TestOutcome> {
    let offerer = run_instance(
        engine,
        component,
        test_id,
        make_config(Role::Offerer, base_url, room, count, size),
    );
    let answerer = run_instance(
        engine,
        component,
        test_id,
        make_config(Role::Answerer, base_url, room, count, size),
    );
    let (offerer, answerer) = futures::join!(offerer, answerer);
    Ok(fold_two(offerer?, answerer?))
}

/// The retry policy for this target: the loopback ICE handshake occasionally
/// stalls or times out waiting to connect; each attempt uses fresh peer
/// connections and a fresh room.
const RETRY: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    attempt_timeout: Duration::from_secs(45),
    is_flaky: default_is_flaky,
};

/// Run one test to a raw result, retrying flaky handshakes with fresh rooms.
async fn run_test(
    engine: &Engine,
    component: &Component,
    base_url: &str,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);

    run_with_retries(test_id, &RETRY, async || {
        let room = format!(
            "conf-{}-{}",
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );
        match plan_for(test_id) {
            Plan::TwoPeer => {
                run_two_peer(engine, component, test_id, base_url, &room, count, size).await
            }
            Plan::InProcess => {
                run_instance(
                    engine,
                    component,
                    test_id,
                    make_config(Role::Both, base_url, &room, count, size),
                )
                .await
            }
            Plan::Skip => {
                run_instance(
                    engine,
                    component,
                    test_id,
                    make_config(Role::Offerer, base_url, &room, count, size),
                )
                .await
            }
        }
    })
    .await
}

// ----- CLI ------------------------------------------------------------------

/// Run the conformance guest against the wasmtime host and emit a result doc.
#[derive(Debug, Parser)]
#[command(name = "conformance-adapter-wasmtime", version)]
struct Cli {
    /// Path to the conformance guest component (`*.component.wasm`).
    #[arg(
        long,
        default_value = "conformance/guest/build/conformance-guest.component.wasm"
    )]
    guest: PathBuf,

    /// Directory to write the adapter result document (`<target>.json`) into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// Target id, matching the manifest `[target].id`.
    #[arg(long, default_value = "wasmtime")]
    target: String,

    /// Environment/scenario label recorded in the result document.
    #[arg(long, default_value = "loopback")]
    environment: String,

    /// Run only these test ids (repeatable). When empty, run every test.
    #[arg(long = "only")]
    only: Vec<String>,

    /// How many tests to run concurrently. Each test's peers use their own
    /// signaling room and ephemeral ports, so tests are independent; the
    /// default keeps the loopback handshakes lightly loaded.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let engine = build_engine()?;
    let component = Component::from_file(&engine, &cli.guest)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading guest component {}", cli.guest.display()))?;

    // Start the signaling server in-process on an ephemeral localhost port.
    let server = conformance_adapter_common::start_signaling_server().await?;
    let base_url = server.base_url();

    let room_seq = AtomicU64::new(0);
    let results = run_corpus(TESTS, &cli.only, cli.jobs, |test_id| {
        run_test(&engine, &component, &base_url, test_id, &room_seq)
    })
    .await;

    server.shutdown().await;

    let report = AdapterReport {
        target: cli.target.clone(),
        environment: cli.environment,
        results,
    };
    write_report(&cli.out, &cli.target, &report)?;

    Ok(())
}
