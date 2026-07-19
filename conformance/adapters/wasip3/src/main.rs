//! Conformance adapter for the `wasip3-guest` target.
//!
//! The target runs the WebRTC stack entirely in wasm: the shared conformance
//! guest composed (`wac plug`) with the `wasip3-impl` provider, an in-guest
//! `wasi:http` mailbox client, and a CLI driver exporting an async
//! `wasi:cli/run`. For each registered test this adapter:
//!
//! - decides how many guest instances the test needs — a single `both` instance
//!   stands up both peers inside one `wasmtime run` (no external signaling) for
//!   the peer-connection API tests, or two `wasmtime run` processes (an
//!   `offerer` and an `answerer`) share one signaling room for the behavioral
//!   tests, connecting over `wasi:sockets` UDP loopback across processes;
//! - parses each process's single-line JSON `test-result` from stdout and folds
//!   the per-instance results into one raw `pass`/`fail`/`skip`.
//!
//! The guest owns every assertion; the adapter only provisions, orchestrates,
//! and records.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;

use conformance_adapter_wasip3::{
    fold_two, params_for, AdapterReport, PeerOutcome, RawResult, RawStatus, Wasip3Peer,
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

/// The orchestration plan for a test id (mirroring the wasmtime adapter: the
/// guest-level plans are target-independent).
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

// ----- orchestration --------------------------------------------------------

/// Whether a failure detail looks like a retryable loopback flake. Besides the
/// `timed-out` / `wait-connected` handshake stalls every target retries, the
/// in-guest sans-I/O stack occasionally reaches `connected` and then loses the
/// data-channel open (the answerer sees no incoming channel while the offerer's
/// channel closes) — the same upstream `rtc` timing issue tracked in TODO.md
/// item E3, retried here with a fresh room.
fn is_flaky(detail: &str) -> bool {
    detail.contains("timed-out")
        || detail.contains("wait-connected")
        || detail.contains("no incoming data channel")
}

/// The number of connection attempts before a flaky handshake is reported as a
/// failure. Each attempt uses fresh processes and a fresh room. The in-guest
/// sans-I/O stack stalls more often than the native hosts (TODO.md item E3), so
/// this target gets a couple more attempts than the wasmtime adapter's three.
const MAX_ATTEMPTS: u32 = 5;

/// How long a single attempt may run before it is abandoned as a stalled
/// handshake and retried (mirroring the wasmtime adapter's guard).
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(45);

/// Run one test to a raw result, retrying flaky handshakes with fresh rooms.
async fn run_test(
    peer: &Wasip3Peer,
    base_url: &str,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);
    let mut last_detail = None;

    for _ in 0..MAX_ATTEMPTS {
        let room = format!(
            "wasip3-{}-{}",
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );
        let attempt = async {
            match plan_for(test_id) {
                Plan::TwoPeer => {
                    let offerer = peer.run(base_url, test_id, &room, "offerer", count, size);
                    let answerer = peer.run(base_url, test_id, &room, "answerer", count, size);
                    let (offerer, answerer) = tokio::join!(offerer, answerer);
                    Ok(fold_two(offerer?, answerer?))
                }
                Plan::InProcess => peer.run(base_url, test_id, &room, "both", count, size).await,
                Plan::Skip => {
                    peer.run(base_url, test_id, &room, "offerer", count, size)
                        .await
                }
            }
        };
        let result: Result<PeerOutcome> = match tokio::time::timeout(ATTEMPT_TIMEOUT, attempt)
            .await
        {
            Ok(result) => result,
            // A stalled attempt is treated like a flaky handshake: retry with a
            // fresh room rather than hanging the whole run.
            Err(_) => {
                last_detail = Some("attempt timed-out".to_string());
                continue;
            }
        };

        match result {
            Ok(PeerOutcome::Pass) => {
                return RawResult {
                    test_id: test_id.to_string(),
                    status: RawStatus::Pass,
                    detail: None,
                }
            }
            Ok(PeerOutcome::Skipped(reason)) => {
                return RawResult {
                    test_id: test_id.to_string(),
                    status: RawStatus::Skip,
                    detail: Some(reason),
                }
            }
            Ok(PeerOutcome::Fail(detail)) => {
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

/// Run the composed wasip3-guest component under `wasmtime run` and emit a
/// result doc.
#[derive(Debug, Parser)]
#[command(name = "conformance-adapter-wasip3", version)]
struct Cli {
    /// Path to the fully composed component (guest + provider + mailbox +
    /// driver; see `just build-conformance-wasip3`).
    #[arg(
        long,
        default_value = "conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm"
    )]
    component: PathBuf,

    /// Directory to write the adapter result document (`<target>.json`) into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// Target id, matching the manifest `[target].id`.
    #[arg(long, default_value = "wasip3-guest")]
    target: String,

    /// Environment/scenario label recorded in the result document.
    #[arg(long, default_value = "loopback")]
    environment: String,

    /// The `wasmtime` binary that runs the composed component (v46+).
    #[arg(long, env = "CONFORMANCE_WASMTIME", default_value = "wasmtime")]
    wasmtime_bin: String,

    /// Run only these test ids (repeatable). When empty, run every test.
    #[arg(long = "only")]
    only: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    anyhow::ensure!(
        cli.component.exists(),
        "composed component {} not found (run `just build-conformance-wasip3`)",
        cli.component.display()
    );
    let peer = Wasip3Peer {
        wasmtime_bin: cli.wasmtime_bin.clone(),
        component: cli.component.clone(),
    };

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
        let result = run_test(&peer, &base_url, test_id, &room_seq).await;
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
