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

use anyhow::Result;
use clap::Parser;

use conformance_adapter_common::{
    fold_two, params_for, plan_for, run_corpus, run_with_retries, write_report, AdapterReport,
    Plan, RawResult, RetryPolicy, TESTS,
};
use conformance_adapter_wasip3::Wasip3Peer;

// ----- orchestration --------------------------------------------------------

/// Whether a failure detail looks like a retryable loopback flake. Besides the
/// `timed-out` / `wait-connected` handshake stalls every target retries, the
/// in-guest sans-I/O stack occasionally reaches `connected` and then loses the
/// data-channel open (the answerer sees no incoming channel while the offerer's
/// channel closes) — the same upstream `rtc` timing issue tracked in TODO.md
/// item E3, retried here with a fresh room.
fn is_flaky(detail: &str) -> bool {
    conformance_adapter_common::default_is_flaky(detail)
        || detail.contains("no incoming data channel")
}

/// The retry policy for this target: the in-guest sans-I/O stack stalls more
/// often than the native hosts (TODO.md item E3), so it gets a couple more
/// attempts than the wasmtime adapter's three; the per-attempt guard mirrors
/// the wasmtime adapter's.
const RETRY: RetryPolicy = RetryPolicy {
    max_attempts: 5,
    attempt_timeout: Duration::from_secs(45),
    is_flaky,
};

/// Run one test to a raw result, retrying flaky handshakes with fresh rooms.
async fn run_test(
    peer: &Wasip3Peer,
    base_url: &str,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);

    run_with_retries(test_id, &RETRY, async || {
        let room = format!(
            "wasip3-{}-{}",
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );
        match plan_for(test_id) {
            Plan::TwoPeer => {
                let offerer = peer.run(base_url, test_id, &room, "offerer", count, size);
                let answerer = peer.run(base_url, test_id, &room, "answerer", count, size);
                let (offerer, answerer) = tokio::join!(offerer, answerer);
                Ok(fold_two(offerer?, answerer?))
            }
            Plan::InProcess => {
                peer.run(base_url, test_id, &room, "both", count, size)
                    .await
            }
            Plan::Skip => {
                peer.run(base_url, test_id, &room, "offerer", count, size)
                    .await
            }
        }
    })
    .await
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

    /// How many tests to run concurrently. Each test's peers are separate
    /// `wasmtime run` processes with their own signaling room and ephemeral
    /// ports, so tests are independent; the default keeps the number of
    /// concurrent processes (two per test) modest.
    #[arg(long, default_value_t = 4)]
    jobs: usize,
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
    let server = conformance_adapter_common::start_signaling_server().await?;
    let base_url = server.base_url();

    let room_seq = AtomicU64::new(0);
    let results = run_corpus(TESTS, &cli.only, cli.jobs, |test_id| {
        run_test(&peer, &base_url, test_id, &room_seq)
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
