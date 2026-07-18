//! Cross-runtime interop orchestrator for the conformance suite.
//!
//! It runs the first interop pair — `wasmtime` <-> `jco-node` — in both orders.
//! One peer is a native wasmtime guest instance (provisioned by
//! [`conformance_adapter_wasmtime`]) and the other is a jco-node guest instance
//! driven out-of-process via `conformance/adapters/jco/run-node.mjs --interop`.
//! Both peers share one in-process `conformance-signalingd` room and connect over
//! a real WebRTC data channel, so a green result proves the two runtimes are
//! genuinely interoperable — not merely that each passes against itself.
//!
//! It writes one adapter result document per direction
//! (`wasmtime-x-jco-node.json`, `jco-node-x-wasmtime.json`) that the conformance
//! runner classifies against the matching manifests, exactly like a single-target
//! adapter.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use serde_json::Value;
use wasmtime::component::Component;
use wasmtime::Engine;

use conformance_adapter_wasmtime::{
    build_engine, fold_two, make_config, params_for, run_instance, AdapterReport, RawResult,
    RawStatus, Role, TestResult,
};

/// The interop corpus subset: the two-peer behavioral tests both runtimes
/// support over loopback. The peer-connection API tests are in-process
/// (single-runtime) and the streaming / remaining error-taxonomy tests are
/// guest-skipped, so neither is meaningful across a runtime boundary.
const INTEROP_TESTS: &[&str] = &[
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

const MAX_ATTEMPTS: u32 = 3;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(45);

/// One direction of the pair: which role the wasmtime peer plays (the jco-node
/// peer plays the other), and the target id the result document records.
struct Direction {
    target: &'static str,
    wasmtime_role: Role,
}

/// The jco-node peer's role, given the wasmtime peer's role.
fn peer_role(wasmtime_role: Role) -> &'static str {
    match wasmtime_role {
        Role::Offerer => "answerer",
        Role::Answerer => "offerer",
        Role::Both => "answerer",
    }
}

/// Run the jco-node peer for one test/role/room via `run-node.mjs --interop`,
/// parsing its single-line JSON `test-result` from stdout.
async fn run_jco_peer(
    cli: &Cli,
    base_url: &str,
    test_id: &str,
    room: &str,
    role: &str,
    count: u32,
    size: u32,
) -> Result<TestResult> {
    let output = tokio::process::Command::new(&cli.node_bin)
        .arg("--experimental-wasm-jspi")
        .arg(&cli.jco_run_node)
        .arg("--interop")
        .args(["--server", base_url])
        .args(["--test", test_id])
        .args(["--room", room])
        .args(["--role", role])
        .args(["--message-count", &count.to_string()])
        .args(["--message-size", &size.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .await
        .with_context(|| format!("spawning jco-node peer ({})", cli.node_bin))?;
    if !output.status.success() {
        anyhow::bail!("jco-node peer exited with {}", output.status);
    }
    let stdout = String::from_utf8(output.stdout).context("jco-node peer stdout not UTF-8")?;
    let line = stdout
        .lines()
        .last()
        .context("jco-node peer produced no result line")?;
    let value: Value = serde_json::from_str(line)
        .with_context(|| format!("parsing jco-node peer result {line:?}"))?;
    parse_result(&value)
}

/// Map a jco `test-result` JSON value (`{ "tag": ..., "val"? }`) to a [`TestResult`].
fn parse_result(value: &Value) -> Result<TestResult> {
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
        "pass" => TestResult::Pass,
        "fail" => TestResult::Fail(detail()),
        "skipped" => TestResult::Skipped(detail()),
        other => anyhow::bail!("unknown result tag {other:?}"),
    })
}

/// Fold results into the raw offerer/answerer order the [`fold_two`] helper
/// expects, then classify: any fail loses, else any skip, else pass.
fn fold_pair(wasmtime_role: Role, wasmtime: TestResult, jco: TestResult) -> TestResult {
    match wasmtime_role {
        Role::Offerer | Role::Both => fold_two(wasmtime, jco),
        Role::Answerer => fold_two(jco, wasmtime),
    }
}

/// Run one interop test to a raw result, retrying flaky handshakes with fresh
/// rooms (mirroring the single-target adapters).
async fn run_interop_test(
    cli: &Cli,
    engine: &Engine,
    component: &Component,
    base_url: &str,
    direction: &Direction,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);
    let jco_role = peer_role(direction.wasmtime_role);
    let mut last_detail = None;

    for _ in 0..MAX_ATTEMPTS {
        let room = format!(
            "interop-{}-{}-{}",
            direction.target,
            test_id,
            room_seq.fetch_add(1, Ordering::SeqCst)
        );

        let wasmtime_peer = run_instance(
            engine,
            component,
            test_id,
            make_config(direction.wasmtime_role, base_url, &room, count, size),
        );
        let jco_peer = run_jco_peer(cli, base_url, test_id, &room, jco_role, count, size);

        let attempt = async { tokio::join!(wasmtime_peer, jco_peer) };
        let (wasmtime_result, jco_result) =
            match tokio::time::timeout(ATTEMPT_TIMEOUT, attempt).await {
                Ok(pair) => pair,
                Err(_) => {
                    last_detail = Some("attempt timed-out".to_string());
                    continue;
                }
            };

        let wasmtime_result = match wasmtime_result {
            Ok(result) => result,
            Err(err) => {
                last_detail = Some(format!("wasmtime peer error: {err:#}"));
                break;
            }
        };
        let jco_result = match jco_result {
            Ok(result) => result,
            Err(err) => {
                last_detail = Some(format!("jco-node peer error: {err:#}"));
                break;
            }
        };

        match fold_pair(direction.wasmtime_role, wasmtime_result, jco_result) {
            TestResult::Pass => {
                return RawResult {
                    test_id: test_id.to_string(),
                    status: RawStatus::Pass,
                    detail: None,
                }
            }
            TestResult::Skipped(reason) => {
                return RawResult {
                    test_id: test_id.to_string(),
                    status: RawStatus::Skip,
                    detail: Some(reason),
                }
            }
            TestResult::Fail(detail) => {
                let flaky = detail.contains("timed-out") || detail.contains("wait-connected");
                last_detail = Some(detail);
                if !flaky {
                    break;
                }
            }
        }
    }

    RawResult {
        test_id: test_id.to_string(),
        status: RawStatus::Fail,
        detail: last_detail,
    }
}

/// Run the wasmtime <-> jco-node interop pair in both orders.
#[derive(Debug, Parser)]
#[command(name = "conformance-interop", version)]
struct Cli {
    /// Path to the conformance guest component (`*.component.wasm`).
    #[arg(
        long,
        default_value = "conformance/guest/build/conformance-guest.component.wasm"
    )]
    guest: PathBuf,

    /// Directory to write the adapter result documents into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// Environment/scenario label recorded in the result documents.
    #[arg(long, default_value = "loopback")]
    environment: String,

    /// The Node binary that drives the jco-node peer. Must be JSPI-capable
    /// (Node 24+). Overridable so CI can point at a specific toolchain node.
    #[arg(long, env = "CONFORMANCE_NODE", default_value = "node")]
    node_bin: String,

    /// Path to the jco-node adapter's `run-node.mjs`.
    #[arg(long, default_value = "conformance/adapters/jco/run-node.mjs")]
    jco_run_node: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let engine = build_engine()?;
    let component = Component::from_file(&engine, &cli.guest)
        .map_err(anyhow::Error::from)
        .with_context(|| format!("loading guest component {}", cli.guest.display()))?;

    let server = conformance_signalingd::spawn(
        "127.0.0.1:0".parse().expect("valid loopback address"),
        conformance_signalingd::Config::default(),
    )
    .await
    .context("starting in-process signaling server")?;
    let base_url = server.base_url();
    eprintln!("signaling server ready at {base_url}");

    let directions = [
        Direction {
            target: "wasmtime-x-jco-node",
            wasmtime_role: Role::Offerer,
        },
        Direction {
            target: "jco-node-x-wasmtime",
            wasmtime_role: Role::Answerer,
        },
    ];

    std::fs::create_dir_all(&cli.out)
        .with_context(|| format!("creating results dir {}", cli.out.display()))?;

    let room_seq = AtomicU64::new(0);
    for direction in &directions {
        eprintln!("== interop {} ==", direction.target);
        let mut results = Vec::with_capacity(INTEROP_TESTS.len());
        for test_id in INTEROP_TESTS {
            eprint!("running {test_id} … ");
            let result = run_interop_test(
                &cli, &engine, &component, &base_url, direction, test_id, &room_seq,
            )
            .await;
            eprintln!("{:?}", result.status);
            results.push(result);
        }

        let report = AdapterReport {
            target: direction.target.to_string(),
            environment: cli.environment.clone(),
            results,
        };
        let out_path = cli.out.join(format!("{}.json", direction.target));
        let json = serde_json::to_string_pretty(&report)?;
        std::fs::write(&out_path, json)
            .with_context(|| format!("writing {}", out_path.display()))?;
        eprintln!("wrote {}", out_path.display());
    }

    server.shutdown().await;
    Ok(())
}
