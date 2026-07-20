//! Target-independent building blocks shared by the native conformance
//! adapters and orchestrators (the wasmtime and wasip3-guest adapters, the
//! cross-runtime interop orchestrator, and the ICE-lab orchestrator).
//!
//! Every adapter follows the same shape — pick each test's orchestration plan,
//! run its guest instances (in-process or as peer subprocesses), retry flaky
//! handshakes with fresh rooms, and record raw `pass`/`fail`/`skip` outcomes in
//! an adapter result document the conformance runner classifies. This crate
//! owns that shared shape:
//!
//! - the WIT-observable [`TestOutcome`] with [`fold_two`] / [`parse_outcome`],
//! - the raw result document types ([`RawResult`], [`RawStatus`],
//!   [`AdapterReport`]) and [`write_report`],
//! - the test registry ([`TESTS`], [`TWO_PEER_TESTS`]) with per-test
//!   orchestration plans ([`plan_for`]) and message parameters ([`params_for`]),
//! - the peer-subprocess invocation ([`run_peer_command`]) with its subtle but
//!   load-bearing process plumbing,
//! - the flaky-handshake retry loop ([`RetryPolicy`], [`run_with_retries`]),
//! - the bounded-concurrency corpus runner ([`run_corpus`]), and
//! - the in-process signaling server startup ([`start_signaling_server`]).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use serde::Serialize;
use serde_json::Value;

// ----- test outcomes ---------------------------------------------------------

/// The WIT-observable outcome of one guest instance: the target-independent
/// mirror of the guest's `test-result` (`conformance/wit/world.wit`).
#[derive(Debug, Clone)]
pub enum TestOutcome {
    Pass,
    Fail(String),
    Skipped(String),
}

impl TestOutcome {
    /// The single-line JSON `test-result` shape peer processes print to stdout
    /// and [`parse_outcome`] parses back: `{ "tag": ..., "val"? }`.
    pub fn to_json(&self) -> Value {
        match self {
            TestOutcome::Pass => serde_json::json!({ "tag": "pass" }),
            TestOutcome::Fail(detail) => serde_json::json!({ "tag": "fail", "val": detail }),
            TestOutcome::Skipped(reason) => {
                serde_json::json!({ "tag": "skipped", "val": reason })
            }
        }
    }
}

/// Map a peer's `test-result` JSON value (`{ "tag": ..., "val"? }`) to a
/// [`TestOutcome`].
pub fn parse_outcome(value: &Value) -> Result<TestOutcome> {
    let tag = value
        .get("tag")
        .and_then(Value::as_str)
        .context("result missing tag")?;
    let detail = || {
        value
            .get("val")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(match tag {
        "pass" => TestOutcome::Pass,
        "fail" => TestOutcome::Fail(detail()),
        "skipped" => TestOutcome::Skipped(detail()),
        other => anyhow::bail!("unknown result tag {other:?}"),
    })
}

/// Fold two per-instance outcomes into one: any fail loses, else any skip,
/// else pass.
pub fn fold_two(offerer: TestOutcome, answerer: TestOutcome) -> TestOutcome {
    match (offerer, answerer) {
        (TestOutcome::Fail(a), TestOutcome::Fail(b)) => {
            TestOutcome::Fail(format!("offerer: {a}; answerer: {b}"))
        }
        (TestOutcome::Fail(a), _) => TestOutcome::Fail(format!("offerer: {a}")),
        (_, TestOutcome::Fail(b)) => TestOutcome::Fail(format!("answerer: {b}")),
        (TestOutcome::Skipped(a), _) => TestOutcome::Skipped(a),
        (_, TestOutcome::Skipped(b)) => TestOutcome::Skipped(b),
        (TestOutcome::Pass, TestOutcome::Pass) => TestOutcome::Pass,
    }
}

// ----- adapter result document -----------------------------------------------

/// The raw status vocabulary the runner consumes (`runner/src/results.rs`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawStatus {
    Pass,
    Fail,
    Skip,
}

/// One raw per-test outcome (`runner/src/results.rs::RawResult`).
#[derive(Debug, Clone, Serialize)]
pub struct RawResult {
    pub test_id: String,
    pub status: RawStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The adapter result document (`runner/src/results.rs::AdapterReport`).
#[derive(Debug, Clone, Serialize)]
pub struct AdapterReport {
    pub target: String,
    pub environment: String,
    pub results: Vec<RawResult>,
}

/// Write `report` to `<out_dir>/<file_stem>.json`, creating `out_dir` as
/// needed, and log the path.
pub fn write_report(out_dir: &Path, file_stem: &str, report: &AdapterReport) -> Result<PathBuf> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating results dir {}", out_dir.display()))?;
    let out_path = out_dir.join(format!("{file_stem}.json"));
    std::fs::write(&out_path, serde_json::to_string_pretty(report)?)
        .with_context(|| format!("writing {}", out_path.display()))?;
    eprintln!("wrote {}", out_path.display());
    Ok(out_path)
}

// ----- test registry & planning ----------------------------------------------

/// The registry of test ids, mirroring `conformance/tests.toml`.
pub const TESTS: &[&str] = &[
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

/// The two-peer behavioral subset of [`TESTS`]: the tests whose outcome depends
/// on a working data channel between two independent peers. This is the corpus
/// the interop pairs and the ICE lab run — the peer-connection API tests are
/// in-process (single-runtime) and the streaming / remaining error-taxonomy
/// tests are guest-skipped, so neither exercises a runtime boundary or a
/// candidate path.
pub const TWO_PEER_TESTS: &[&str] = &[
    "label-round-trip",
    "binary-message",
    "text-message",
    "message-boundaries",
    "zero-length-message",
    "large-message",
    "ordering",
    "payload-integrity",
    "concurrent-send-receive",
    "max-retransmits-accepted",
    "interop-handshake",
];

/// How a test is orchestrated across guest instances.
pub enum Plan {
    /// A single `both` instance stands up both peers in-process (no signaling).
    InProcess,
    /// A single instance that the guest reports `skipped` regardless of role.
    Skip,
    /// Two instances — an offerer and an answerer — share one signaling room.
    TwoPeer,
}

/// The orchestration plan for a test id. The plans are target-independent: the
/// guest decides what each test does with the role it is given.
pub fn plan_for(test_id: &str) -> Plan {
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

/// The `(message-count, message-size)` a test runs with.
pub fn params_for(test_id: &str) -> (u32, u32) {
    match test_id {
        "large-message" => (1, 16384),
        "message-boundaries"
        | "ordering"
        | "payload-integrity"
        | "concurrent-send-receive"
        | "interop-handshake" => (16, 512),
        _ => (4, 256),
    }
}

// ----- peer subprocess invocation --------------------------------------------

/// Run one peer subprocess to a [`TestOutcome`], parsing its single-line JSON
/// `test-result` from stdout. `label` names the peer in error details.
///
/// This owns the subtle process plumbing every out-of-process peer needs:
/// stdin is closed, stderr flows through to the orchestrator's, and — crucially
/// — the child is killed if this future is dropped (`kill_on_drop`), so an
/// attempt abandoned by [`run_with_retries`]' timeout reaps its peers instead
/// of leaking them to hold TURN allocations and CPU across attempts.
pub async fn run_peer_command(
    mut command: tokio::process::Command,
    label: &str,
) -> Result<TestOutcome> {
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("spawning {label}"))?;
    if !output.status.success() {
        anyhow::bail!("{label} exited with {}", output.status);
    }
    let stdout =
        String::from_utf8(output.stdout).with_context(|| format!("{label} stdout not UTF-8"))?;
    let line = stdout
        .lines()
        .last()
        .with_context(|| format!("{label} produced no result line"))?;
    let value: Value =
        serde_json::from_str(line).with_context(|| format!("parsing {label} result {line:?}"))?;
    parse_outcome(&value)
}

// ----- flaky-handshake retry loop --------------------------------------------

/// How [`run_with_retries`] treats a test's attempts: how many to make, how
/// long each may run before it is abandoned as a stalled handshake, and which
/// failure details look like retryable flakes.
pub struct RetryPolicy {
    /// Attempts before a flaky handshake is reported as a failure. Each attempt
    /// runs fresh instances/processes with a fresh room.
    pub max_attempts: u32,
    /// How long a single attempt may run before it is abandoned and retried. It
    /// must exceed the host's `wait-connected` timeout so a genuine connection
    /// failure surfaces as a WIT outcome rather than tripping this guard.
    pub attempt_timeout: Duration,
    /// Whether a failure detail looks like a retryable flake.
    pub is_flaky: fn(&str) -> bool,
}

/// Whether a failure detail looks like a retryable handshake flake: the
/// loopback/lab ICE handshake occasionally stalls or times out waiting to
/// connect. Targets with additional known flake modes wrap this predicate.
pub fn default_is_flaky(detail: &str) -> bool {
    detail.contains("timed-out") || detail.contains("wait-connected")
}

/// Run one test to a [`RawResult`], retrying flaky attempts per `policy`.
///
/// `attempt` runs one full attempt (allocating its own fresh room) to the
/// folded [`TestOutcome`] of its instances. An attempt that outlives
/// `policy.attempt_timeout` is dropped — cancelling in-process instances and
/// killing peer subprocesses (see [`run_peer_command`]) — and retried; a
/// non-flaky failure or an orchestration error ends the test immediately.
pub async fn run_with_retries(
    test_id: &str,
    policy: &RetryPolicy,
    mut attempt: impl AsyncFnMut() -> Result<TestOutcome>,
) -> RawResult {
    let mut last_detail = None;

    for _ in 0..policy.max_attempts {
        let outcome = match tokio::time::timeout(policy.attempt_timeout, attempt()).await {
            Ok(outcome) => outcome,
            // A stalled attempt is treated like a flaky handshake: retry with a
            // fresh room rather than hanging the whole run.
            Err(_) => {
                last_detail = Some("attempt timed-out".to_string());
                continue;
            }
        };

        match outcome {
            Ok(TestOutcome::Pass) => {
                return RawResult {
                    test_id: test_id.to_string(),
                    status: RawStatus::Pass,
                    detail: None,
                }
            }
            Ok(TestOutcome::Skipped(reason)) => {
                return RawResult {
                    test_id: test_id.to_string(),
                    status: RawStatus::Skip,
                    detail: Some(reason),
                }
            }
            Ok(TestOutcome::Fail(detail)) => {
                let flaky = (policy.is_flaky)(&detail);
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

// ----- corpus runner ----------------------------------------------------------

/// Run `tests` (each via `run`, filtered by `only` when non-empty) concurrently,
/// bounded by `jobs`, logging each result as it lands.
///
/// Tests are independent — fresh instances/processes, a fresh room per attempt
/// — so they can safely overlap; `buffered` preserves the registry order of the
/// results.
pub async fn run_corpus<F, Fut>(
    tests: &[&'static str],
    only: &[String],
    jobs: usize,
    run: F,
) -> Vec<RawResult>
where
    F: Fn(&'static str) -> Fut,
    Fut: Future<Output = RawResult>,
{
    futures::stream::iter(
        tests
            .iter()
            .copied()
            .filter(|test_id| only.is_empty() || only.iter().any(|t| t == test_id)),
    )
    .map(|test_id| {
        let fut = run(test_id);
        async move {
            let result = fut.await;
            eprintln!("{test_id} … {:?}", result.status);
            result
        }
    })
    .buffered(jobs.max(1))
    .collect()
    .await
}

// ----- signaling server -------------------------------------------------------

/// Start the in-process signaling server on an ephemeral localhost port and log
/// its base URL.
pub async fn start_signaling_server() -> Result<conformance_signalingd::RunningServer> {
    let server = conformance_signalingd::spawn(
        "127.0.0.1:0".parse().expect("valid loopback address"),
        conformance_signalingd::Config::default(),
    )
    .await
    .context("starting in-process signaling server")?;
    eprintln!("signaling server ready at {}", server.base_url());
    Ok(server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_json_round_trips() {
        for outcome in [
            TestOutcome::Pass,
            TestOutcome::Fail("boom".to_string()),
            TestOutcome::Skipped("later".to_string()),
        ] {
            let parsed = parse_outcome(&outcome.to_json()).unwrap();
            match (&outcome, &parsed) {
                (TestOutcome::Pass, TestOutcome::Pass) => {}
                (TestOutcome::Fail(a), TestOutcome::Fail(b)) if a == b => {}
                (TestOutcome::Skipped(a), TestOutcome::Skipped(b)) if a == b => {}
                other => panic!("round trip mismatch: {other:?}"),
            }
        }
        assert!(parse_outcome(&serde_json::json!({ "tag": "nope" })).is_err());
        assert!(parse_outcome(&serde_json::json!({})).is_err());
    }

    #[test]
    fn fold_two_prefers_fail_then_skip() {
        let fail = || TestOutcome::Fail("f".to_string());
        let skip = || TestOutcome::Skipped("s".to_string());
        assert!(matches!(
            fold_two(fail(), TestOutcome::Pass),
            TestOutcome::Fail(d) if d == "offerer: f"
        ));
        assert!(matches!(
            fold_two(skip(), fail()),
            TestOutcome::Fail(d) if d == "answerer: f"
        ));
        assert!(matches!(
            fold_two(skip(), TestOutcome::Pass),
            TestOutcome::Skipped(_)
        ));
        assert!(matches!(
            fold_two(TestOutcome::Pass, TestOutcome::Pass),
            TestOutcome::Pass
        ));
    }

    #[test]
    fn two_peer_tests_are_registered_two_peer_plans() {
        for test_id in TWO_PEER_TESTS {
            assert!(TESTS.contains(test_id), "{test_id} missing from TESTS");
            assert!(
                matches!(plan_for(test_id), Plan::TwoPeer),
                "{test_id} is not a two-peer plan"
            );
        }
    }
}
