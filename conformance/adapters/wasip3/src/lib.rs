//! Shared building blocks for the wasip3-guest conformance adapter and the
//! cross-runtime interop orchestrator.
//!
//! The wasip3-guest target runs the conformance suite entirely in wasm: the
//! shared conformance guest is composed (`wac plug`) with the `wasip3-impl`
//! provider (which exports `connections` over WASIp3 `wasi:sockets`), an
//! in-guest `wasi:http` mailbox client, and a CLI driver that exports an async
//! `wasi:cli/run`. One `wasmtime run` invocation of that composed component
//! runs one guest instance of one test and reports its raw `test-result` as a
//! single JSON line on stdout.
//!
//! This library provides [`Wasip3Peer`] — the primitive to run one such
//! instance and parse its outcome — plus the raw result / report types the
//! adapter binary serializes. The `conformance-interop` binary (in
//! `adapters/wasmtime`) reuses [`Wasip3Peer`] to drive the wasip3 half of the
//! `wasmtime` <-> `wasip3-guest` interop pair.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context as _, Result};
use serde::Serialize;
use serde_json::Value;

/// The WIT-observable outcome of one guest instance, parsed from the driver's
/// JSON result line (`{"tag": "pass" | "fail" | "skipped", "val"?}`).
#[derive(Debug, Clone)]
pub enum PeerOutcome {
    Pass,
    Fail(String),
    Skipped(String),
}

/// One raw per-test outcome (mirrors `runner/src/results.rs::RawResult`).
#[derive(Debug, Clone, Serialize)]
pub struct RawResult {
    pub test_id: String,
    pub status: RawStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The raw status of one test.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawStatus {
    Pass,
    Fail,
    Skip,
}

/// The adapter result document (mirrors `runner/src/results.rs::AdapterReport`).
#[derive(Debug, Clone, Serialize)]
pub struct AdapterReport {
    pub target: String,
    pub environment: String,
    pub results: Vec<RawResult>,
}

/// A runner for one wasip3-guest peer: the composed component plus the
/// `wasmtime` binary that executes it.
pub struct Wasip3Peer {
    /// The `wasmtime` binary (v46+; must support `-W component-model-async` and
    /// `-S p3`).
    pub wasmtime_bin: String,
    /// The fully composed component (guest + provider + mailbox + driver).
    pub component: PathBuf,
}

impl Wasip3Peer {
    /// Run one guest instance of `test_id` as `role` against `room` on the
    /// signaling server at `server`, parsing the driver's single-line JSON
    /// `test-result` from stdout.
    ///
    /// The flags mirror `just test-webrtc-composed` plus `-S http` for the
    /// in-guest mailbox client: the component-model async ABI, the WASIp3 host
    /// APIs, and network access for the provider's `wasi:sockets` UDP and the
    /// mailbox's outgoing HTTP.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        &self,
        server: &str,
        test_id: &str,
        room: &str,
        role: &str,
        count: u32,
        size: u32,
    ) -> Result<PeerOutcome> {
        let output = tokio::process::Command::new(&self.wasmtime_bin)
            .arg("run")
            .args(["-W", "component-model-async=y"])
            .args([
                "-S",
                "cli",
                "-S",
                "p3",
                "-S",
                "http",
                "-S",
                "inherit-network",
            ])
            .arg(&self.component)
            .args(["--test", test_id])
            .args(["--role", role])
            .args(["--server", server])
            .args(["--room", room])
            .args(["--message-count", &count.to_string()])
            .args(["--message-size", &size.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            // Reap the peer if the attempt times out and this future is dropped.
            .kill_on_drop(true)
            .output()
            .await
            .with_context(|| format!("spawning wasip3-guest peer ({})", self.wasmtime_bin))?;
        if !output.status.success() {
            anyhow::bail!("wasip3-guest peer exited with {}", output.status);
        }
        let stdout =
            String::from_utf8(output.stdout).context("wasip3-guest peer stdout not UTF-8")?;
        let line = stdout
            .lines()
            .last()
            .context("wasip3-guest peer produced no result line")?;
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("parsing wasip3-guest peer result {line:?}"))?;
        parse_outcome(&value)
    }
}

/// Map a driver `test-result` JSON value (`{ "tag": ..., "val"? }`) to a
/// [`PeerOutcome`].
fn parse_outcome(value: &Value) -> Result<PeerOutcome> {
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
        "pass" => PeerOutcome::Pass,
        "fail" => PeerOutcome::Fail(detail()),
        "skipped" => PeerOutcome::Skipped(detail()),
        other => anyhow::bail!("unknown result tag {other:?}"),
    })
}

/// Fold two per-instance outcomes into one: any fail loses, else any skip,
/// else pass (mirroring the wasmtime adapter's `fold_two`).
pub fn fold_two(offerer: PeerOutcome, answerer: PeerOutcome) -> PeerOutcome {
    match (offerer, answerer) {
        (PeerOutcome::Fail(a), PeerOutcome::Fail(b)) => {
            PeerOutcome::Fail(format!("offerer: {a}; answerer: {b}"))
        }
        (PeerOutcome::Fail(a), _) => PeerOutcome::Fail(format!("offerer: {a}")),
        (_, PeerOutcome::Fail(b)) => PeerOutcome::Fail(format!("answerer: {b}")),
        (PeerOutcome::Skipped(a), _) => PeerOutcome::Skipped(a),
        (_, PeerOutcome::Skipped(b)) => PeerOutcome::Skipped(b),
        (PeerOutcome::Pass, PeerOutcome::Pass) => PeerOutcome::Pass,
    }
}

/// The `(message-count, message-size)` a test runs with (mirroring the
/// wasmtime adapter's `params_for`).
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
