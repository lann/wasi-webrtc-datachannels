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

use conformance_adapter_wasmtime::{
    build_engine, fold_two, make_config, params_for, run_instance, AdapterReport, RawResult,
    RawStatus, Role, TestResult,
};

// ----- test planning --------------------------------------------------------

/// How a test is orchestrated across guest instances.
enum Plan {
    /// A single `both` instance stands up both peers in-process (no signaling).
    InProcess,
    /// A single instance that the guest reports `skipped` regardless of role.
    Skip,
    /// Two instances — an offerer and an answerer — share one signaling room.
    TwoPeer,
}

/// The orchestration plan for a test id.
fn plan_for(test_id: &str) -> Plan {
    match test_id {
        "peer-offer-answer"
        | "peer-create-data-channel"
        | "peer-local-ice-candidates"
        | "peer-add-ice-candidate"
        | "peer-wait-connected"
        | "peer-close-releases"
        | "peer-invalid-sdp"
        | "error-invalid-signaling" => Plan::InProcess,
        "send-via-stream"
        | "receive-via-stream"
        | "receive-via-stream-once"
        | "post-close-send"
        | "error-closed"
        | "error-timed-out" => Plan::Skip,
        _ => Plan::TwoPeer,
    }
}

/// The registry of test ids, mirroring `conformance/tests.toml`.
const TESTS: &[&str] = &[
    "label-round-trip",
    "binary-message",
    "text-message",
    "message-boundaries",
    "zero-length-message",
    "large-message",
    "ordering",
    "payload-integrity",
    "concurrent-send-receive",
    "send-via-stream",
    "receive-via-stream",
    "receive-via-stream-once",
    "post-close-send",
    "max-retransmits-accepted",
    "error-invalid-signaling",
    "error-closed",
    "error-timed-out",
    "peer-offer-answer",
    "peer-create-data-channel",
    "peer-local-ice-candidates",
    "peer-add-ice-candidate",
    "peer-wait-connected",
    "peer-close-releases",
    "peer-invalid-sdp",
    "interop-handshake",
];

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
) -> Result<TestResult> {
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

/// Whether a failure detail looks like a retryable loopback-ICE flake.
fn is_flaky(detail: &str) -> bool {
    detail.contains("timed-out") || detail.contains("wait-connected")
}

/// The number of connection attempts before a flaky handshake is reported as a
/// failure. The loopback ICE handshake occasionally stalls; each attempt uses
/// fresh peer connections and a fresh room.
const MAX_ATTEMPTS: u32 = 3;

/// How long a single attempt may run before it is abandoned as a stalled
/// handshake and retried. It must exceed the host's `wait-connected` timeout so
/// a genuine connection failure surfaces as a WIT outcome rather than tripping
/// this guard, while still bounding an attempt whose data-channel wait never
/// resolves (e.g. a peer whose channel never opens).
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(45);

/// Run one test to a raw result, retrying flaky handshakes with fresh rooms.
async fn run_test(
    engine: &Engine,
    component: &Component,
    base_url: &str,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);
    let mut last_detail = None;

    for _ in 0..MAX_ATTEMPTS {
        let room = format!(
            "conf-{}-{}",
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );
        let attempt = async {
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
        };
        let result = match tokio::time::timeout(ATTEMPT_TIMEOUT, attempt).await {
            Ok(result) => result,
            // A stalled attempt is treated like a flaky handshake: retry with a
            // fresh room rather than hanging the whole run.
            Err(_) => {
                last_detail = Some("attempt timed-out".to_string());
                continue;
            }
        };

        match result {
            Ok(TestResult::Pass) => {
                return RawResult {
                    test_id: test_id.to_string(),
                    status: RawStatus::Pass,
                    detail: None,
                }
            }
            Ok(TestResult::Skipped(reason)) => {
                return RawResult {
                    test_id: test_id.to_string(),
                    status: RawStatus::Skip,
                    detail: Some(reason),
                }
            }
            Ok(TestResult::Fail(detail)) => {
                let flaky = is_flaky(&detail);
                last_detail = Some(detail);
                if !flaky {
                    break;
                }
            }
            Err(err) => {
                last_detail = Some(format!("adapter error: {err:#}"));
                break;
            }
        }
    }

    RawResult {
        test_id: test_id.to_string(),
        status: RawStatus::Fail,
        detail: last_detail,
    }
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let engine = build_engine()?;
    let component = Component::from_file(&engine, &cli.guest)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading guest component {}", cli.guest.display()))?;

    // Start the signaling server in-process on an ephemeral localhost port.
    let server = conformance_signalingd::spawn(
        "127.0.0.1:0".parse().expect("valid loopback address"),
        conformance_signalingd::Config::default(),
    )
    .await
    .context("starting in-process signaling server")?;
    let base_url = server.base_url();
    eprintln!("signaling server ready at {base_url}");

    let room_seq = AtomicU64::new(0);
    let mut results = Vec::with_capacity(TESTS.len());
    for test_id in TESTS {
        if !cli.only.is_empty() && !cli.only.iter().any(|t| t == test_id) {
            continue;
        }
        eprint!("running {test_id} … ");
        let result = run_test(&engine, &component, &base_url, test_id, &room_seq).await;
        eprintln!("{:?}", result.status);
        results.push(result);
    }

    server.shutdown().await;

    let report = AdapterReport {
        target: cli.target.clone(),
        environment: cli.environment,
        results,
    };

    std::fs::create_dir_all(&cli.out)
        .with_context(|| format!("creating results dir {}", cli.out.display()))?;
    let out_path = cli.out.join(format!("{}.json", cli.target));
    let json = serde_json::to_string_pretty(&report)?;
    std::fs::write(&out_path, json).with_context(|| format!("writing {}", out_path.display()))?;
    eprintln!("wrote {}", out_path.display());

    Ok(())
}
