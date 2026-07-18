//! Conformance adapter for the wasmtime (native `webrtc-rs`) target.
//!
//! It runs the shared conformance guest component against the wasmtime host
//! ([`wasmtime_webrtc_datachannels`]) and emits an adapter result document the
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
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Parser;
use serde::Serialize;
use wasmtime::component::{Accessor, Component, HasData, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_webrtc_datachannels::{
    self as webrtc_host, WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView,
};

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "conformance",
        imports: {
            default: async | store | trappable,
        },
        exports: {
            default: async,
        },
        with: {
            "lann:webrtc-datachannels/connections.data-channel-options":
                wasmtime_webrtc_datachannels::DataChannelOptions,
            "lann:webrtc-datachannels/connections.data-channel":
                wasmtime_webrtc_datachannels::DataChannel,
            "lann:webrtc-datachannels/connections.peer-connection":
                wasmtime_webrtc_datachannels::PeerConnection,
            "conformance:signaling/mailbox.session": super::MailboxSession,
        },
    });
}

use bindings::conformance::signaling::mailbox::{self, Role as MailboxRole};
use bindings::exports::conformance::suite::runner::{Role, TestConfig, TestResult};
use bindings::lann::webrtc_datachannels::types::Error;
use bindings::Conformance;

/// Per-store host state: the WebRTC host context plus the resource table shared
/// by the WebRTC and mailbox host resources.
struct Ctx {
    webrtc: WasiWebrtcCtx,
    table: ResourceTable,
}

impl HasData for Ctx {
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

// ----- mailbox host ---------------------------------------------------------

/// A joined mailbox session: an HTTP client bound to one `{room}` and `{role}`
/// on the in-process signaling server. `Arc`-backed so a handle can be cloned
/// out of the resource table and its async methods driven without holding the
/// store borrow across `.await` (mirroring the WebRTC host's data channel).
#[derive(Clone)]
pub struct MailboxSession {
    client: reqwest::Client,
    base: String,
    room: String,
    role: MailboxRole,
    /// The next sequence number to fetch from the peer's mailbox.
    recv_seq: Arc<AtomicUsize>,
}

impl MailboxSession {
    /// This session's own role path segment.
    fn own_role(&self) -> &'static str {
        role_str(self.role)
    }

    /// The peer's role path segment (the mailbox this session consumes).
    fn peer_role(&self) -> &'static str {
        match self.role {
            MailboxRole::Offerer => "answerer",
            MailboxRole::Answerer => "offerer",
        }
    }
}

/// The path segment for a mailbox role.
fn role_str(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Offerer => "offerer",
        MailboxRole::Answerer => "answerer",
    }
}

/// Map any host-side mailbox failure to the guest-visible `error.other`.
fn mailbox_error(detail: impl std::fmt::Display) -> Error {
    Error::Other(format!("mailbox: {detail}"))
}

impl mailbox::Host for Ctx {}

impl mailbox::HostSession for Ctx {}

impl mailbox::HostSessionWithStore<Ctx> for Ctx {
    async fn open(
        accessor: &Accessor<Ctx, Ctx>,
        server: String,
        room: String,
        as_role: MailboxRole,
    ) -> wasmtime::Result<std::result::Result<Resource<MailboxSession>, Error>> {
        let session = MailboxSession {
            client: reqwest::Client::new(),
            base: server.trim_end_matches('/').to_string(),
            room,
            role: as_role,
            recv_seq: Arc::new(AtomicUsize::new(0)),
        };
        accessor.with(|mut access| {
            let resource = access.get().table.push(session)?;
            Ok(Ok(resource))
        })
    }

    async fn send(
        accessor: &Accessor<Ctx, Ctx>,
        self_: Resource<MailboxSession>,
        blob: Vec<u8>,
    ) -> wasmtime::Result<std::result::Result<(), Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        let url = format!(
            "{}/rooms/{}/{}",
            session.base,
            session.room,
            session.own_role()
        );
        Ok(match session.client.post(&url).body(blob).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(mailbox_error(format!("publish status {}", resp.status()))),
            Err(err) => Err(mailbox_error(err)),
        })
    }

    async fn recv(
        accessor: &Accessor<Ctx, Ctx>,
        self_: Resource<MailboxSession>,
    ) -> wasmtime::Result<std::result::Result<Option<Vec<u8>>, Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        Ok(fetch_next(&session).await)
    }

    async fn done(
        accessor: &Accessor<Ctx, Ctx>,
        self_: Resource<MailboxSession>,
    ) -> wasmtime::Result<std::result::Result<(), Error>> {
        let session = accessor
            .with(|mut access| Ok::<_, wasmtime::Error>(access.get().table.get(&self_)?.clone()))?;
        let url = format!(
            "{}/rooms/{}/{}/done",
            session.base,
            session.room,
            session.own_role()
        );
        Ok(match session.client.post(&url).send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(mailbox_error(format!("done status {}", resp.status()))),
            Err(err) => Err(mailbox_error(err)),
        })
    }

    async fn drop(
        accessor: &Accessor<Ctx, Ctx>,
        rep: Resource<MailboxSession>,
    ) -> wasmtime::Result<()> {
        accessor.with(|mut access| {
            access.get().table.delete(rep)?;
            Ok(())
        })
    }
}

/// Fetch the next blob from the peer's mailbox, long-polling and retrying `304`
/// until a blob arrives (`some`) or the peer marks its mailbox done (`none`).
async fn fetch_next(session: &MailboxSession) -> std::result::Result<Option<Vec<u8>>, Error> {
    loop {
        let seq = session.recv_seq.load(Ordering::SeqCst);
        let url = format!(
            "{}/rooms/{}/{}?seq={}&wait=10000",
            session.base,
            session.room,
            session.peer_role(),
            seq
        );
        let resp = session
            .client
            .get(&url)
            .send()
            .await
            .map_err(mailbox_error)?;
        match resp.status().as_u16() {
            // A blob is available: advance our read cursor and return it.
            200 => {
                let bytes = resp.bytes().await.map_err(mailbox_error)?.to_vec();
                session.recv_seq.store(seq + 1, Ordering::SeqCst);
                return Ok(Some(bytes));
            }
            // The peer marked its mailbox done at or before this seq.
            204 => return Ok(None),
            // Not yet available; retry the same seq.
            304 => continue,
            other => return Err(mailbox_error(format!("fetch status {other}"))),
        }
    }
}

/// Add the native mailbox host to `linker`.
fn add_mailbox_to_linker(linker: &mut Linker<Ctx>) -> Result<()> {
    mailbox::add_to_linker::<Ctx, Ctx>(linker, |c| c)?;
    Ok(())
}

// ----- adapter result document ---------------------------------------------

/// The raw status vocabulary the runner consumes (`results.rs::RawStatus`).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum RawStatus {
    Pass,
    Fail,
    Skip,
}

/// One raw per-test outcome (`results.rs::RawResult`).
#[derive(Debug, Clone, Serialize)]
struct RawResult {
    test_id: String,
    status: RawStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// The adapter result document (`results.rs::AdapterReport`).
#[derive(Debug, Clone, Serialize)]
struct AdapterReport {
    target: String,
    environment: String,
    results: Vec<RawResult>,
}

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

/// The `(message-count, message-size)` a test runs with.
fn params_for(test_id: &str) -> (u32, u32) {
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

/// A wasmtime engine configured for the component model with async support.
fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Ok(Engine::new(&config)?)
}

/// A fresh store whose WebRTC host restricts ICE to loopback so two same-host
/// peers pair deterministically. `set_include_loopback_candidate` gathers the
/// loopback candidates and the IP filter drops every non-loopback address, so
/// the peers avoid the flaky LAN / container / link-local candidate pairs that
/// otherwise stall the handshake.
fn new_store(engine: &Engine) -> Store<Ctx> {
    let mut webrtc = WasiWebrtcCtx::new();
    webrtc.set_setting_engine_hook(|engine| {
        engine.set_include_loopback_candidate(true);
        engine.set_ip_filter(Box::new(|ip| ip.is_loopback()));
    });
    Store::new(
        engine,
        Ctx {
            webrtc,
            table: ResourceTable::new(),
        },
    )
}

/// Build a test config for one instance.
fn make_config(role: Role, base_url: &str, room: &str, count: u32, size: u32) -> TestConfig {
    TestConfig {
        role,
        signaling_server: base_url.to_string(),
        room: room.to_string(),
        message_count: count,
        message_size: size,
        trickle: true,
    }
}

/// Run one guest instance to a [`TestResult`].
async fn run_instance(
    engine: &Engine,
    component: &Component,
    test_id: &str,
    config: TestConfig,
) -> Result<TestResult> {
    let mut linker: Linker<Ctx> = Linker::new(engine);
    webrtc_host::add_to_linker(&mut linker)?;
    add_mailbox_to_linker(&mut linker)?;

    let mut store = new_store(engine);
    let instance = Conformance::instantiate_async(&mut store, component, &linker).await?;
    let test_id = test_id.to_string();
    let result = store
        .run_concurrent(async move |accessor: &Accessor<Ctx>| {
            instance
                .conformance_suite_runner()
                .call_run_test(accessor, test_id, config)
                .await
        })
        .await??;
    Ok(result)
}

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

/// Fold two per-instance results into one: any fail loses, else any skip, else
/// pass.
fn fold_two(offerer: TestResult, answerer: TestResult) -> TestResult {
    match (offerer, answerer) {
        (TestResult::Fail(a), TestResult::Fail(b)) => {
            TestResult::Fail(format!("offerer: {a}; answerer: {b}"))
        }
        (TestResult::Fail(a), _) => TestResult::Fail(format!("offerer: {a}")),
        (_, TestResult::Fail(b)) => TestResult::Fail(format!("answerer: {b}")),
        (TestResult::Skipped(a), _) => TestResult::Skipped(a),
        (_, TestResult::Skipped(b)) => TestResult::Skipped(b),
        (TestResult::Pass, TestResult::Pass) => TestResult::Pass,
    }
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
