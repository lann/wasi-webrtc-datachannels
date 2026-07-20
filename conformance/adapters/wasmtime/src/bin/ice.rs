//! The conformance ICE-lab environment executor.
//!
//! It runs the two-peer behavioral corpus for one target in one ICE scenario
//! (`lan`, `stun-srflx`, `turn-relay`, `nat-symmetric`; see
//! `conformance/PLAN.md` Phases 5 and 6) over a provisioned network-namespace
//! topology. Each test's two peers run as separate `conformance-peer` processes,
//! one placed in the offerer namespace and one in the answerer namespace
//! (`ip netns exec`), so their handshake traverses a real routed path — and, for
//! the server-mediated scenarios, is forced through the STUN/TURN server because
//! the router blocks the direct peer-to-peer path (and, for the NAT scenarios,
//! rewrites each peer's address).
//!
//! The signaling server (and, for `turn-relay`/`stun-srflx`, coturn) run in the
//! signaling namespace, reachable from both peers through the router. The result
//! document it writes (`<target>-<scenario>.json`, `environment = <scenario>`) is
//! classified by the conformance runner exactly like any other adapter report.
//!
//! Requires root (for `ip netns exec`) and a lab provisioned by
//! `conformance/scenarios/scenario.sh` — which this binary drives itself unless
//! `--no-provision` is passed.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;

use conformance_adapter_common::{
    default_is_flaky, fold_two, params_for, run_peer_command, run_with_retries, write_report,
    AdapterReport, RawResult, RetryPolicy, TestOutcome, TWO_PEER_TESTS,
};
use conformance_adapter_wasmtime::{LabConfig, Role, Scenario};

/// The retry policy for the lab. Lab handshakes (real routing, and a TURN relay
/// for `turn-relay`) are slower to establish than loopback, so the per-attempt
/// guard is more generous than the loopback adapters' 45s.
const RETRY: RetryPolicy = RetryPolicy {
    max_attempts: 3,
    attempt_timeout: Duration::from_secs(60),
    is_flaky: default_is_flaky,
};

#[derive(Debug, Parser)]
#[command(name = "conformance-ice", version)]
struct Cli {
    /// Scenario to run (`lan`, `stun-srflx`, `turn-relay`, `nat-symmetric`).
    #[arg(long)]
    scenario: String,

    /// Target id recorded in the result document (only `wasmtime` is supported
    /// by this orchestrator so far; the peer binary is the wasmtime host).
    #[arg(long, default_value = "wasmtime")]
    target: String,

    /// Path to the conformance guest component (`*.component.wasm`).
    #[arg(
        long,
        default_value = "conformance/guest/build/conformance-guest.component.wasm"
    )]
    guest: PathBuf,

    /// Directory to write the adapter result document into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// Directory holding the scenario provisioning scripts.
    #[arg(long, default_value = "conformance/scenarios")]
    scenarios_dir: PathBuf,

    /// The `conformance-signalingd` binary, started inside the signaling
    /// namespace.
    #[arg(long, default_value = "target/debug/conformance-signalingd")]
    signaling_bin: PathBuf,

    /// The `conformance-peer` binary, launched inside each peer namespace.
    #[arg(long, default_value = "target/release/conformance-peer")]
    peer_bin: PathBuf,

    /// Offerer / answerer / signaling namespace names (must match the lab).
    #[arg(long, default_value = "cw-off")]
    offerer_ns: String,
    #[arg(long, default_value = "cw-ans")]
    answerer_ns: String,
    #[arg(long, default_value = "cw-sig")]
    signaling_ns: String,

    /// Peer bind addresses and the signaling/STUN/TURN server address.
    #[arg(long, default_value = "10.79.1.2")]
    offerer_addr: String,
    #[arg(long, default_value = "10.79.2.2")]
    answerer_addr: String,
    #[arg(long, default_value = "10.79.3.2")]
    server_addr: String,

    /// Signaling HTTP port and TURN/STUN port in the signaling namespace. Each
    /// test uses its own signaling server on a distinct port (base + index) so
    /// tests stay independent and can run concurrently.
    #[arg(long, default_value_t = 8080)]
    signaling_port: u16,
    #[arg(long, default_value_t = 3478)]
    turn_port: u16,

    /// TURN long-term credentials (must match coturn's config).
    #[arg(long, default_value = "conf")]
    turn_user: String,
    #[arg(long, default_value = "conf")]
    turn_pass: String,

    /// Provision (and tear down) the lab via `scenario.sh`. On by default; pass
    /// `--no-provision` when the lab is already up (e.g. interactive debugging).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    provision: bool,

    /// Run only these test ids (repeatable). When empty, run the whole corpus.
    #[arg(long = "only")]
    only: Vec<String>,

    /// How many tests to run concurrently. Each test's peers use their own
    /// signaling room, so tests are independent.
    #[arg(long, default_value_t = 2)]
    jobs: usize,
}

impl Cli {
    fn lab(&self) -> LabConfig {
        LabConfig {
            offerer_addr: self.offerer_addr.clone(),
            answerer_addr: self.answerer_addr.clone(),
            server_addr: format!("{}:{}", self.server_addr, self.turn_port),
            turn_user: self.turn_user.clone(),
            turn_pass: self.turn_pass.clone(),
        }
    }
}

/// A live child process wrapped so it is killed when dropped (used for the
/// signaling server and, when provisioned, the lab teardown on exit).
struct Guard {
    child: Option<std::process::Child>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let scenario = Scenario::parse(&cli.scenario)?;

    if cli.provision {
        run_script(&cli, &["up", scenario.as_str()])
            .with_context(|| format!("provisioning scenario {}", scenario.as_str()))?;
    }
    // Ensure the lab is torn down even if the run fails, when we provisioned it.
    let _teardown = TeardownOnDrop {
        cli: &cli,
        enabled: cli.provision,
    };

    // Each test starts its own short-lived signaling server (in the signaling
    // namespace, on its own port) around its handshake, so a server only ever
    // brokers one room — matching the mailbox's per-room lifecycle.
    run_corpus(&cli, scenario).await
}

/// Run the whole corpus for the scenario and write the result document.
async fn run_corpus(cli: &Cli, scenario: Scenario) -> Result<()> {
    let lab = cli.lab();
    let room_seq = AtomicU64::new(0);

    eprintln!(
        "== ice {} / {} (offerer={} answerer={}) ==",
        cli.target,
        scenario.as_str(),
        cli.offerer_addr,
        cli.answerer_addr
    );

    let results =
        conformance_adapter_common::run_corpus(TWO_PEER_TESTS, &cli.only, cli.jobs, |test_id| {
            run_ice_test(cli, scenario, &lab, test_id, &room_seq)
        })
        .await;

    let report = AdapterReport {
        target: cli.target.clone(),
        environment: scenario.as_str().to_string(),
        results,
    };
    write_report(
        &cli.out,
        &format!("{}-{}", cli.target, scenario.as_str()),
        &report,
    )?;
    Ok(())
}

/// Run one test to a raw result, retrying flaky handshakes with fresh rooms.
///
/// Each attempt gets its own signaling server (in the signaling namespace, on
/// its own port) brokering a single room, so a server never has to survive more
/// than one handshake and concurrent tests never share one.
async fn run_ice_test(
    cli: &Cli,
    scenario: Scenario,
    lab: &LabConfig,
    test_id: &str,
    room_seq: &AtomicU64,
) -> RawResult {
    let (count, size) = params_for(test_id);

    run_with_retries(test_id, &RETRY, async || {
        let n = room_seq.fetch_add(1, Ordering::SeqCst);
        let room = format!("ice-{}-{}-{}", scenario.as_str(), test_id, n);
        let port = cli.signaling_port.wrapping_add(n as u16);
        let signaling_url = format!("http://{}:{}", cli.server_addr, port);

        // Bring up this attempt's signaling server; killed when `_signaling`
        // drops at the end of the attempt.
        let _signaling = start_signaling(cli, port).context("signaling server")?;
        // Let it bind before the peers connect; peer-side long-poll retries cover
        // any residual race.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let offerer = run_peer(
            cli,
            scenario,
            lab,
            &signaling_url,
            test_id,
            &room,
            Role::Offerer,
            count,
            size,
        );
        let answerer = run_peer(
            cli,
            scenario,
            lab,
            &signaling_url,
            test_id,
            &room,
            Role::Answerer,
            count,
            size,
        );

        let (offerer, answerer) = tokio::join!(offerer, answerer);
        Ok(fold_two(offerer?, answerer?))
    })
    .await
}

/// Run one peer of a test inside its namespace, parsing its single-line JSON
/// `test-result` from stdout.
#[allow(clippy::too_many_arguments)]
async fn run_peer(
    cli: &Cli,
    scenario: Scenario,
    lab: &LabConfig,
    signaling_url: &str,
    test_id: &str,
    room: &str,
    role: Role,
    count: u32,
    size: u32,
) -> Result<TestOutcome> {
    let ns = match role {
        Role::Answerer => &cli.answerer_ns,
        Role::Offerer | Role::Both => &cli.offerer_ns,
    };
    let role_str = match role {
        Role::Offerer | Role::Both => "offerer",
        Role::Answerer => "answerer",
    };
    let ice = scenario.ice_config(role, lab);
    let bind_addr = scenario.bind_addr(role, lab);

    let mut command = tokio::process::Command::new("ip");
    command
        .args(["netns", "exec", ns])
        .arg(&cli.peer_bin)
        .args(["--guest", &cli.guest.to_string_lossy()])
        .args(["--test", test_id])
        .args(["--role", role_str])
        .args(["--server", signaling_url])
        .args(["--room", room])
        .args(["--message-count", &count.to_string()])
        .args(["--message-size", &size.to_string()])
        .args(["--bind-addr", &bind_addr]);
    if let Some(server) = ice.ice_servers.first() {
        command
            .args(["--ice-server-url", &server.urls[0]])
            .args(["--ice-username", &server.username])
            .args(["--ice-credential", &server.credential]);
    }
    if ice.relay_only {
        command.arg("--relay-only");
    }

    run_peer_command(command, &format!("{role_str} peer in {ns}")).await
}

/// Start a signaling server on `port` inside the signaling namespace.
fn start_signaling(cli: &Cli, port: u16) -> Result<Guard> {
    let child = std::process::Command::new("ip")
        .args(["netns", "exec", &cli.signaling_ns])
        .arg(&cli.signaling_bin)
        .args(["--host", &cli.server_addr])
        .args(["--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "spawning signaling server ({}) in {}",
                cli.signaling_bin.display(),
                cli.signaling_ns
            )
        })?;
    Ok(Guard { child: Some(child) })
}

/// Run `scenario.sh` with `args`, inheriting stdio, failing on a nonzero exit.
fn run_script(cli: &Cli, args: &[&str]) -> Result<()> {
    let script = cli.scenarios_dir.join("scenario.sh");
    let status = std::process::Command::new("bash")
        .arg(&script)
        .args(args)
        .status()
        .with_context(|| format!("running {} {:?}", script.display(), args))?;
    if !status.success() {
        anyhow::bail!("{} {:?} exited with {status}", script.display(), args);
    }
    Ok(())
}

/// Tears the lab down (via `scenario.sh down`) when dropped, so a provisioned lab
/// is always cleaned up regardless of how the run ends.
struct TeardownOnDrop<'a> {
    cli: &'a Cli,
    enabled: bool,
}

impl Drop for TeardownOnDrop<'_> {
    fn drop(&mut self) {
        if self.enabled {
            let _ = run_script(self.cli, &["down"]);
        }
    }
}
