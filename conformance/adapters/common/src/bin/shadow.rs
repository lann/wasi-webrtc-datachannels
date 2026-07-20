//! The conformance Shadow-lab environment executor.
//!
//! It runs the two-peer behavioral corpus for one target inside the
//! [Shadow](https://github.com/shadow/shadow) network simulator, which gives the
//! same "two peers on separate hosts over a routed, non-loopback path" property
//! as the netns ICE lab (`conformance-ice`) but **without root, network
//! namespaces, or a real kernel network stack**: Shadow runs every process under
//! a deterministic, simulated network, intercepting their syscalls. That makes
//! this environment reproducible and runnable in restricted sandboxes and in CI
//! where raw `ip netns` traffic is unavailable.
//!
//! Unlike the netns executor — which `exec`s each peer into a namespace and
//! drives the corpus itself with retries — Shadow owns the whole run: this binary
//! generates a single Shadow configuration describing, for each test, a signaling
//! host plus an offerer and an answerer host (each on its own simulated IP), runs
//! `shadow` once, then reads each peer's single-line JSON `test-result` from the
//! per-host stdout file Shadow captures. The run is deterministic, so no retries
//! are needed. The result document it writes (`<target>-shadow.json`,
//! `environment = shadow`) is classified by the conformance runner exactly like
//! any other adapter report.
//!
//! The executor is target-neutral: `--peer-kind` selects how each peer host's
//! command line is built. Any peer that honours the shared single-peer contract
//! (`--test`/`--role`/`--server`/`--room`/…, one JSON result line on stdout) and
//! can bind a configured non-loopback address fits:
//!
//! - `wasmtime` runs the native `conformance-peer` binary. Its peers gather host
//!   candidates from their simulated interface address (`--bind-addr`) and run
//!   with `--disable-mdns`: Shadow's simulated stack does not implement the
//!   multicast-socket options (`SO_REUSEADDR`/`SO_REUSEPORT`) that multicast-DNS
//!   candidate gathering binds with, and the peers connect over their explicit
//!   host candidates rather than `.local` names, so mDNS is unnecessary here.
//! - `wasip3-guest` runs the fully composed wasip3 conformance component under
//!   `wasmtime run` (the same invocation as the loopback adapter), pointing the
//!   in-guest provider at the host's simulated address through the
//!   `WEBRTC_UDP_BIND_ADDR` environment variable. The sans-I/O stack has no
//!   mDNS, so no equivalent of `--disable-mdns` is needed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context as _, Result};
use clap::Parser;

use conformance_adapter_common::{
    fold_two, params_for, parse_outcome, write_report, AdapterReport, RawResult, RawStatus,
    TestOutcome, TWO_PEER_TESTS,
};

/// How a peer host's command line is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum PeerKind {
    /// The native `conformance-peer` binary (the wasmtime host + webrtc-rs).
    Wasmtime,
    /// The composed wasip3 conformance component under `wasmtime run`.
    Wasip3Guest,
}

#[derive(Debug, Parser)]
#[command(name = "conformance-shadow", version)]
struct Cli {
    /// Target id recorded in the result document, matching the manifest
    /// `[target].id`.
    #[arg(long, default_value = "wasmtime")]
    target: String,

    /// How each peer host's command line is built (which target's peer runs).
    #[arg(long, value_enum, default_value_t = PeerKind::Wasmtime)]
    peer_kind: PeerKind,

    /// Environment id recorded in the result document (the matrix column).
    #[arg(long, default_value = "shadow")]
    environment: String,

    /// Path to the conformance guest component (`*.component.wasm`), used by
    /// the `wasmtime` peer kind.
    #[arg(
        long,
        default_value = "conformance/guest/build/conformance-guest.component.wasm"
    )]
    guest: PathBuf,

    /// The fully composed wasip3 conformance component (guest + provider +
    /// mailbox + driver), used by the `wasip3-guest` peer kind.
    #[arg(
        long,
        default_value = "conformance/adapters/wasip3/build/conformance-wasip3.composed.wasm"
    )]
    component: PathBuf,

    /// The `wasmtime` binary that runs the composed component (v46+), used by
    /// the `wasip3-guest` peer kind.
    #[arg(long, env = "CONFORMANCE_WASMTIME", default_value = "wasmtime")]
    wasmtime_bin: String,

    /// Directory to write the adapter result document into.
    #[arg(long, default_value = "conformance/results")]
    out: PathBuf,

    /// The `conformance-signalingd` binary, run on each test's signaling host.
    #[arg(long, default_value = "target/debug/conformance-signalingd")]
    signaling_bin: PathBuf,

    /// The `conformance-peer` binary, run on each peer host by the `wasmtime`
    /// peer kind.
    #[arg(long, default_value = "target/release/conformance-peer")]
    peer_bin: PathBuf,

    /// The `shadow` simulator binary.
    #[arg(long, default_value = "shadow")]
    shadow_bin: PathBuf,

    /// Directory Shadow writes its per-host output into. It is removed and
    /// recreated on each run (Shadow refuses to overwrite an existing one).
    #[arg(long, default_value = "target/shadow-data")]
    data_dir: PathBuf,

    /// Simulated stop time. Handshakes settle in a few simulated seconds; this
    /// only bounds a stuck run.
    #[arg(long, default_value = "300s")]
    stop_time: String,

    /// How many worker cores Shadow may use.
    #[arg(long, default_value_t = 4)]
    parallelism: usize,

    /// Signaling HTTP port used on every signaling host (each test has its own
    /// host, so the port never collides).
    #[arg(long, default_value_t = 8080)]
    signaling_port: u16,

    /// Run only these test ids (repeatable). When empty, run the whole corpus.
    #[arg(long = "only")]
    only: Vec<String>,
}

/// One two-peer test placed on three simulated hosts.
struct Placement {
    test_id: &'static str,
    index: usize,
    signaling_ip: String,
    offerer_ip: String,
    answerer_ip: String,
    count: u32,
    size: u32,
}

impl Placement {
    fn new(test_id: &'static str, index: usize) -> Self {
        let (count, size) = params_for(test_id);
        Self {
            test_id,
            index,
            signaling_ip: format!("11.0.{index}.1"),
            offerer_ip: format!("11.0.{index}.2"),
            answerer_ip: format!("11.0.{index}.3"),
            count,
            size,
        }
    }

    fn signaling_url(&self, port: u16) -> String {
        format!("http://{}:{}", self.signaling_ip, port)
    }
}

/// The resolved per-role peer command template: how one peer host's process is
/// invoked for a role/IP of a placement.
enum PeerCommand {
    /// `conformance-peer --guest … --bind-addr <ip> --disable-mdns …`
    Wasmtime { peer_bin: PathBuf, guest: PathBuf },
    /// `wasmtime run … --env WEBRTC_UDP_BIND_ADDR=<ip> <component> …`
    Wasip3Guest {
        wasmtime_bin: PathBuf,
        component: PathBuf,
    },
}

impl PeerCommand {
    /// Resolve the selected peer kind's binaries and components to absolute
    /// paths.
    fn resolve(cli: &Cli) -> Result<Self> {
        Ok(match cli.peer_kind {
            PeerKind::Wasmtime => PeerCommand::Wasmtime {
                peer_bin: absolute(&cli.peer_bin)?,
                guest: absolute(&cli.guest)?,
            },
            PeerKind::Wasip3Guest => PeerCommand::Wasip3Guest {
                wasmtime_bin: resolve_bin(&cli.wasmtime_bin)?,
                component: absolute(&cli.component)?,
            },
        })
    }

    /// The full argv (element 0 is the executable) for one peer process, each
    /// element rendered as a double-quoted YAML/JSON scalar.
    fn argv(&self, p: &Placement, role: &str, ip: &str, signaling_url: &str) -> Vec<String> {
        let shared_peer_args = [
            json_str("--test"),
            json_str(p.test_id),
            json_str("--role"),
            json_str(role),
            json_str("--server"),
            json_str(signaling_url),
            json_str("--room"),
            json_str("r"),
            json_str("--message-count"),
            json_str(&p.count.to_string()),
            json_str("--message-size"),
            json_str(&p.size.to_string()),
        ];
        match self {
            PeerCommand::Wasmtime { peer_bin, guest } => {
                let mut args = vec![path_arg(peer_bin), json_str("--guest"), path_arg(guest)];
                args.extend(shared_peer_args);
                args.extend([
                    json_str("--bind-addr"),
                    json_str(ip),
                    json_str("--disable-mdns"),
                ]);
                args
            }
            PeerCommand::Wasip3Guest {
                wasmtime_bin,
                component,
            } => {
                // Mirror the loopback wasip3 adapter's `wasmtime run`
                // invocation, plus the provider's bind-address environment
                // variable pointing it at this host's simulated address.
                let mut args = vec![
                    path_arg(wasmtime_bin),
                    json_str("run"),
                    json_str("-W"),
                    json_str("component-model-async=y"),
                    json_str("-S"),
                    json_str("cli"),
                    json_str("-S"),
                    json_str("p3"),
                    json_str("-S"),
                    json_str("http"),
                    json_str("-S"),
                    json_str("inherit-network"),
                    json_str("--env"),
                    json_str(&format!("WEBRTC_UDP_BIND_ADDR={ip}")),
                    path_arg(component),
                ];
                args.extend(shared_peer_args);
                args
            }
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Shadow runs each managed process with its working directory set to that
    // host's output directory, not the caller's, so every path handed to a
    // process (binaries and components) must be absolute.
    let peer_command = PeerCommand::resolve(&cli)?;
    let signaling_bin = absolute(&cli.signaling_bin)?;

    let placements: Vec<Placement> = TWO_PEER_TESTS
        .iter()
        .filter(|id| cli.only.is_empty() || cli.only.iter().any(|o| o == *id))
        .enumerate()
        .map(|(index, id)| Placement::new(id, index))
        .collect();
    if placements.is_empty() {
        anyhow::bail!("no tests selected");
    }

    let config = render_config(&cli, &peer_command, &signaling_bin, &placements);
    let config_path = cli.data_dir.with_extension("yaml");
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&config_path, &config)
        .with_context(|| format!("writing shadow config {}", config_path.display()))?;

    // Shadow refuses to overwrite an existing data directory, so clear it first.
    if cli.data_dir.exists() {
        std::fs::remove_dir_all(&cli.data_dir)
            .with_context(|| format!("clearing {}", cli.data_dir.display()))?;
    }

    eprintln!(
        "== shadow {} / {} ({} test(s)) ==",
        cli.target,
        cli.environment,
        placements.len()
    );
    run_shadow(&cli, &config_path)?;

    let results: Vec<RawResult> = placements.iter().map(|p| collect_result(&cli, p)).collect();

    let report = AdapterReport {
        target: cli.target.clone(),
        environment: cli.environment.clone(),
        results,
    };
    write_report(
        &cli.out,
        &format!("{}-{}", cli.target, cli.environment),
        &report,
    )?;
    Ok(())
}

/// Render the Shadow configuration placing every selected test on its own trio
/// of simulated hosts (signaling, offerer, answerer).
fn render_config(
    cli: &Cli,
    peer_command: &PeerCommand,
    signaling_bin: &Path,
    placements: &[Placement],
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "general:");
    let _ = writeln!(s, "  stop_time: {}", cli.stop_time);
    // Advance the simulated clock past blocking syscalls so an idle wait (a peer
    // long-polling the mailbox) never spins the wall clock.
    let _ = writeln!(s, "  model_unblocked_syscall_latency: true");
    let _ = writeln!(s, "network:");
    let _ = writeln!(s, "  graph:");
    let _ = writeln!(s, "    type: 1_gbit_switch");
    let _ = writeln!(s, "hosts:");

    for p in placements {
        // Signaling host: a long-lived server, so it is expected to still be
        // running when the simulation stops.
        emit_host(
            &mut s,
            &format!("sig{}", p.index),
            &p.signaling_ip,
            &[
                path_arg(signaling_bin),
                json_str("--host"),
                json_str(&p.signaling_ip),
                json_str("--port"),
                json_str(&cli.signaling_port.to_string()),
            ],
            "0s",
            Some("running"),
        );

        for (role, ip) in [("offerer", &p.offerer_ip), ("answerer", &p.answerer_ip)] {
            emit_host(
                &mut s,
                &format!("{role}{}", p.index),
                ip,
                &peer_command.argv(p, role, ip, &p.signaling_url(cli.signaling_port)),
                // Give the signaling server a moment to bind; peer-side long-poll
                // retries cover any residual race.
                "2s",
                None,
            );
        }
    }
    s
}

/// Emit one Shadow host running a single process.
fn emit_host(
    s: &mut String,
    name: &str,
    ip: &str,
    args: &[String],
    start_time: &str,
    expected_running: Option<&str>,
) {
    let _ = writeln!(s, "  {name}:");
    let _ = writeln!(s, "    ip_addr: {ip}");
    let _ = writeln!(s, "    network_node_id: 0");
    let _ = writeln!(s, "    processes:");
    let _ = writeln!(s, "    - path: {}", args[0]);
    let _ = writeln!(s, "      args: [{}]", args[1..].join(", "));
    let _ = writeln!(s, "      start_time: {start_time}");
    if let Some(state) = expected_running {
        let _ = writeln!(s, "      expected_final_state: {state}");
    }
}

/// A path rendered as a double-quoted YAML/JSON scalar.
fn path_arg(path: &Path) -> String {
    json_str(&path.to_string_lossy())
}

/// Resolve `path` to an absolute path (canonicalizing when it exists), so it
/// survives being handed to a process that runs with a different cwd.
fn absolute(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return Ok(canonical);
    }
    let cwd = std::env::current_dir().context("getting current directory")?;
    Ok(cwd.join(path))
}

/// Resolve a binary named by path or bare name: a bare name (no path
/// separator) is searched for on `PATH`; anything else is made absolute like
/// [`absolute`].
fn resolve_bin(bin: &str) -> Result<PathBuf> {
    if !bin.contains(std::path::MAIN_SEPARATOR) {
        let path = std::env::var_os("PATH").context("reading PATH")?;
        return std::env::split_paths(&path)
            .map(|dir| dir.join(bin))
            .find(|candidate| candidate.is_file())
            .with_context(|| format!("{bin} not found on PATH"));
    }
    absolute(Path::new(bin))
}

/// A string rendered as a double-quoted scalar (valid in YAML flow context).
fn json_str(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// Run `shadow` once over the generated config, inheriting stdio and failing
/// only on spawn errors — a nonzero exit (e.g. a peer that did not reach its
/// expected final state) is diagnosed per-test from the parsed results instead.
fn run_shadow(cli: &Cli, config_path: &Path) -> Result<()> {
    let status = std::process::Command::new(&cli.shadow_bin)
        .args(["--parallelism", &cli.parallelism.to_string()])
        .arg("--data-directory")
        .arg(&cli.data_dir)
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("spawning {}", cli.shadow_bin.display()))?;
    if !status.success() {
        // Not fatal: results are read from each host's stdout regardless.
        eprintln!("warning: shadow exited with {status}; classifying per-test results");
    }
    Ok(())
}

/// Read one test's offerer and answerer outcomes and fold them into a raw result.
fn collect_result(cli: &Cli, p: &Placement) -> RawResult {
    let offerer = read_outcome(&cli.data_dir, &format!("offerer{}", p.index));
    let answerer = read_outcome(&cli.data_dir, &format!("answerer{}", p.index));
    let outcome = match (offerer, answerer) {
        (Ok(o), Ok(a)) => fold_two(o, a),
        (Err(e), _) | (_, Err(e)) => {
            TestOutcome::Fail(format!("shadow: missing peer result: {e:#}"))
        }
    };
    to_raw(p.test_id, outcome)
}

/// Parse the single-line JSON `test-result` a peer printed to its stdout, which
/// Shadow captures at `<data_dir>/hosts/<host>/<proc>.<pid>.stdout`.
fn read_outcome(data_dir: &Path, host: &str) -> Result<TestOutcome> {
    let host_dir = data_dir.join("hosts").join(host);
    let stdout_path = std::fs::read_dir(&host_dir)
        .with_context(|| format!("reading {}", host_dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.extension().is_some_and(|e| e == "stdout"))
        .with_context(|| format!("no stdout file in {}", host_dir.display()))?;
    let text = std::fs::read_to_string(&stdout_path)
        .with_context(|| format!("reading {}", stdout_path.display()))?;
    let line = text
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .with_context(|| format!("empty result in {}", stdout_path.display()))?;
    let value: serde_json::Value = serde_json::from_str(line.trim())
        .with_context(|| format!("parsing result {line:?} from {}", stdout_path.display()))?;
    parse_outcome(&value)
}

/// Convert a folded outcome into the runner's raw result vocabulary.
fn to_raw(test_id: &str, outcome: TestOutcome) -> RawResult {
    match outcome {
        TestOutcome::Pass => RawResult {
            test_id: test_id.to_string(),
            status: RawStatus::Pass,
            detail: None,
        },
        TestOutcome::Skipped(reason) => RawResult {
            test_id: test_id.to_string(),
            status: RawStatus::Skip,
            detail: Some(reason),
        },
        TestOutcome::Fail(detail) => RawResult {
            test_id: test_id.to_string(),
            status: RawStatus::Fail,
            detail: Some(detail),
        },
    }
}
