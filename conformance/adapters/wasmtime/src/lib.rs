//! Shared building blocks for the wasmtime conformance adapter and the
//! cross-runtime interop orchestrator.
//!
//! This library provides the wasmtime host provisioning (the [`Ctx`] store
//! state, the WebRTC host via [`wasmtime_webrtc_datachannels`], and the native
//! HTTP [`mailbox`] host backed by `conformance-signalingd`), the guest bindings,
//! and the primitive to run one guest instance to a WIT-observable
//! [`TestOutcome`]. The adapter binary ([`crate::main`]) layers the full-corpus
//! orchestration on top; the `conformance-interop` binary reuses the same
//! primitive to drive one wasmtime peer of an interop pair.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Result;
use wasmtime::component::{Accessor, Component, HasData, Linker, Resource, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_webrtc_datachannels::{
    self as webrtc_host, WasiWebrtcCtx, WasiWebrtcCtxView, WasiWebrtcView,
};

pub mod bindings {
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
            "conformance:signaling/mailbox.session": crate::MailboxSession,
        },
    });
}

use bindings::conformance::signaling::mailbox::{self, Role as MailboxRole};
use bindings::exports::conformance::suite::runner::TestResult;
pub use bindings::exports::conformance::suite::runner::{Role, TestConfig};
use bindings::lann::webrtc_datachannels::types::Error;
pub use bindings::Conformance;
use conformance_adapter_common::TestOutcome;
pub use wasmtime_webrtc_datachannels::{WebrtcIceConfig, WebrtcIceServer};

/// Per-store host state: the WebRTC host context plus the resource table shared
/// by the WebRTC and mailbox host resources.
pub struct Ctx {
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
/// on the signaling server. `Arc`-backed so a handle can be cloned out of the
/// resource table and its async methods driven without holding the store borrow
/// across `.await` (mirroring the WebRTC host's data channel).
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
pub fn add_mailbox_to_linker(linker: &mut Linker<Ctx>) -> Result<()> {
    mailbox::add_to_linker::<Ctx, Ctx>(linker, |c| c)?;
    Ok(())
}

// ----- guest orchestration --------------------------------------------------

/// A wasmtime engine configured for the component model with async support.
pub fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    Ok(Engine::new(&config)?)
}

/// A fresh store whose WebRTC host restricts ICE to loopback so two same-host
/// peers pair deterministically. Peers bind on IPv4 loopback, so only loopback
/// candidates are gathered, and `set_include_loopback_candidate` keeps them
/// rather than discarding them; this avoids the flaky LAN / container /
/// link-local candidate pairs that otherwise stall the handshake.
pub fn new_store(engine: &Engine) -> Store<Ctx> {
    let mut webrtc = WasiWebrtcCtx::new();
    webrtc.set_setting_engine_hook(|engine| {
        engine.set_include_loopback_candidate(true);
    });
    Store::new(
        engine,
        Ctx {
            webrtc,
            table: ResourceTable::new(),
        },
    )
}

/// A fresh store whose WebRTC host uses an explicit [`WebrtcIceConfig`] — the ICE
/// lab's per-scenario network configuration (bind address, STUN/TURN servers,
/// relay-only policy; see `conformance/PLAN.md` Phase 5). Unlike [`new_store`],
/// loopback candidates are *not* forced: lab peers connect over real interface
/// addresses, so gathering the loopback host candidate would only add a
/// never-connectable pair.
///
/// When `disable_mdns` is set, multicast-DNS candidate gathering is turned off.
/// The Shadow environment (`conformance-shadow`) needs this because Shadow's
/// simulated network stack does not implement the multicast-socket options
/// (`SO_REUSEADDR`/`SO_REUSEPORT`) mDNS binds with; the routed netns lab leaves
/// it on, so both environments keep their own behavior.
pub fn new_store_with_ice(engine: &Engine, ice: WebrtcIceConfig, disable_mdns: bool) -> Store<Ctx> {
    let mut webrtc = WasiWebrtcCtx::new();
    webrtc.set_ice_config(ice);
    if disable_mdns {
        webrtc.set_setting_engine_hook(|engine| {
            engine.set_multicast_dns_mode(rtc::ice::mdns::MulticastDnsMode::Disabled);
        });
    }
    Store::new(
        engine,
        Ctx {
            webrtc,
            table: ResourceTable::new(),
        },
    )
}

/// Build a test config for one instance.
pub fn make_config(role: Role, base_url: &str, room: &str, count: u32, size: u32) -> TestConfig {
    TestConfig {
        role,
        signaling_server: base_url.to_string(),
        room: room.to_string(),
        message_count: count,
        message_size: size,
        trickle: true,
    }
}

/// Run one guest instance to a [`TestOutcome`].
pub async fn run_instance(
    engine: &Engine,
    component: &Component,
    test_id: &str,
    config: TestConfig,
) -> Result<TestOutcome> {
    run_instance_in_store(new_store(engine), engine, component, test_id, config).await
}

/// Run one guest instance to a [`TestOutcome`] with an explicit ICE-lab network
/// configuration (bind address, STUN/TURN servers, relay-only policy). Used by
/// the single-peer `conformance-peer` binary the ICE-lab orchestrator launches
/// inside a network namespace.
///
/// `disable_mdns` turns off multicast-DNS candidate gathering; see
/// [`new_store_with_ice`] for why the Shadow environment needs it.
pub async fn run_instance_with_ice(
    engine: &Engine,
    component: &Component,
    test_id: &str,
    config: TestConfig,
    ice: WebrtcIceConfig,
    disable_mdns: bool,
) -> Result<TestOutcome> {
    run_instance_in_store(
        new_store_with_ice(engine, ice, disable_mdns),
        engine,
        component,
        test_id,
        config,
    )
    .await
}

/// Instantiate the guest in `store` and drive one `run-test` call to its outcome.
async fn run_instance_in_store(
    mut store: Store<Ctx>,
    engine: &Engine,
    component: &Component,
    test_id: &str,
    config: TestConfig,
) -> Result<TestOutcome> {
    let mut linker: Linker<Ctx> = Linker::new(engine);
    webrtc_host::add_to_linker(&mut linker)?;
    add_mailbox_to_linker(&mut linker)?;

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
    Ok(match result {
        TestResult::Pass => TestOutcome::Pass,
        TestResult::Fail(detail) => TestOutcome::Fail(detail),
        TestResult::Skipped(reason) => TestOutcome::Skipped(reason),
    })
}

// ----- ICE lab scenarios ----------------------------------------------------

/// The addresses and TURN credentials of a provisioned ICE lab, mirroring the
/// defaults in `conformance/scenarios/lib.sh`. The orchestrator overrides these
/// from CLI flags so the Rust side and the shell provisioning agree on where the
/// peers, signaling server, and STUN/TURN server live.
#[derive(Debug, Clone)]
pub struct LabConfig {
    /// UDP address the offerer peer binds and gathers host candidates from.
    pub offerer_addr: String,
    /// UDP address the answerer peer binds and gathers host candidates from.
    pub answerer_addr: String,
    /// STUN/TURN server URL host:port, e.g. `10.79.3.2:3478`.
    pub server_addr: String,
    /// TURN long-term-credential username.
    pub turn_user: String,
    /// TURN long-term-credential secret.
    pub turn_pass: String,
}

impl Default for LabConfig {
    fn default() -> Self {
        Self {
            offerer_addr: "10.79.1.2".to_string(),
            answerer_addr: "10.79.2.2".to_string(),
            server_addr: "10.79.3.2:3478".to_string(),
            turn_user: "conf".to_string(),
            turn_pass: "conf".to_string(),
        }
    }
}

/// An ICE-lab scenario: the candidate path a run is set up to exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Direct host-candidate connectivity over the router (no STUN/TURN server).
    Lan,
    /// Server-reflexive candidates via a STUN server behind a port-restricted
    /// (cone) NAT; the direct path is blocked, and the cone NAT lets the srflx
    /// candidates connect (see `conformance/PLAN.md` Phase 6).
    StunSrflx,
    /// Relayed candidates via a TURN server; the direct path is blocked and the
    /// peers are relay-only.
    TurnRelay,
    /// A symmetric NAT with a STUN/TURN server available but no relay-only
    /// policy: the direct path is blocked and the symmetric NAT makes the srflx
    /// candidates unusable, so ICE must fall back to a TURN relay (Phase 6).
    NatSymmetric,
}

impl Scenario {
    /// The scenario id used on the command line, in result documents, and as the
    /// matrix environment column.
    pub fn as_str(self) -> &'static str {
        match self {
            Scenario::Lan => "lan",
            Scenario::StunSrflx => "stun-srflx",
            Scenario::TurnRelay => "turn-relay",
            Scenario::NatSymmetric => "nat-symmetric",
        }
    }

    /// Parse a scenario id (`lan`, `stun-srflx`, `turn-relay`, `nat-symmetric`).
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "lan" => Ok(Scenario::Lan),
            "stun-srflx" => Ok(Scenario::StunSrflx),
            "turn-relay" => Ok(Scenario::TurnRelay),
            "nat-symmetric" => Ok(Scenario::NatSymmetric),
            other => anyhow::bail!(
                "unknown scenario {other:?} (lan|stun-srflx|turn-relay|nat-symmetric)"
            ),
        }
    }

    /// The bind address for a peer role in `lab`.
    pub fn bind_addr(self, role: Role, lab: &LabConfig) -> String {
        match role {
            Role::Answerer => lab.answerer_addr.clone(),
            // `both` never appears in the two-peer lab; treat it as the offerer.
            Role::Offerer | Role::Both => lab.offerer_addr.clone(),
        }
    }

    /// The [`WebrtcIceConfig`] a peer of this scenario runs with: it always binds
    /// its scenario interface address, and additionally points at the STUN or
    /// TURN server (and forces relay-only) for the server-mediated scenarios.
    pub fn ice_config(self, role: Role, lab: &LabConfig) -> WebrtcIceConfig {
        let mut ice = WebrtcIceConfig {
            udp_addrs: vec![format!("{}:0", self.bind_addr(role, lab))],
            ..Default::default()
        };
        match self {
            Scenario::Lan => {}
            Scenario::StunSrflx => {
                ice.ice_servers = vec![WebrtcIceServer {
                    urls: vec![format!("stun:{}", lab.server_addr)],
                    ..Default::default()
                }];
            }
            Scenario::TurnRelay => {
                ice.ice_servers = vec![WebrtcIceServer {
                    urls: vec![format!("turn:{}?transport=udp", lab.server_addr)],
                    username: lab.turn_user.clone(),
                    credential: lab.turn_pass.clone(),
                }];
                ice.relay_only = true;
            }
            Scenario::NatSymmetric => {
                // A TURN server (which also serves STUN, so the peer gathers both
                // srflx and relay candidates) with no relay-only policy: under a
                // symmetric NAT the srflx pair fails and ICE falls back to relay.
                ice.ice_servers = vec![WebrtcIceServer {
                    urls: vec![format!("turn:{}?transport=udp", lab.server_addr)],
                    username: lab.turn_user.clone(),
                    credential: lab.turn_pass.clone(),
                }];
            }
        }
        ice
    }
}

#[cfg(test)]
mod scenario_tests {
    use super::*;

    #[test]
    fn scenario_ids_round_trip() {
        for s in [
            Scenario::Lan,
            Scenario::StunSrflx,
            Scenario::TurnRelay,
            Scenario::NatSymmetric,
        ] {
            assert_eq!(Scenario::parse(s.as_str()).unwrap(), s);
        }
        assert!(Scenario::parse("nope").is_err());
    }

    #[test]
    fn lan_binds_role_address_without_servers() {
        let lab = LabConfig::default();
        let off = Scenario::Lan.ice_config(Role::Offerer, &lab);
        assert_eq!(off.udp_addrs, vec!["10.79.1.2:0".to_string()]);
        assert!(off.ice_servers.is_empty());
        assert!(!off.relay_only);
        let ans = Scenario::Lan.ice_config(Role::Answerer, &lab);
        assert_eq!(ans.udp_addrs, vec!["10.79.2.2:0".to_string()]);
    }

    #[test]
    fn stun_srflx_adds_stun_server_only() {
        let ice = Scenario::StunSrflx.ice_config(Role::Offerer, &LabConfig::default());
        assert_eq!(ice.ice_servers.len(), 1);
        assert_eq!(
            ice.ice_servers[0].urls,
            vec!["stun:10.79.3.2:3478".to_string()]
        );
        assert!(ice.ice_servers[0].username.is_empty());
        assert!(!ice.relay_only);
    }

    #[test]
    fn turn_relay_is_relay_only_with_credentials() {
        let ice = Scenario::TurnRelay.ice_config(Role::Answerer, &LabConfig::default());
        assert!(ice.relay_only);
        assert_eq!(ice.ice_servers.len(), 1);
        assert_eq!(
            ice.ice_servers[0].urls,
            vec!["turn:10.79.3.2:3478?transport=udp".to_string()]
        );
        assert_eq!(ice.ice_servers[0].username, "conf");
        assert_eq!(ice.ice_servers[0].credential, "conf");
    }

    #[test]
    fn nat_symmetric_offers_turn_without_relay_only() {
        // The symmetric-NAT scenario configures a TURN server (which also serves
        // STUN) but leaves relay-only off, so the peer gathers srflx *and* relay
        // candidates and falls back to relay when srflx fails under symmetric NAT.
        let ice = Scenario::NatSymmetric.ice_config(Role::Offerer, &LabConfig::default());
        assert!(!ice.relay_only);
        assert_eq!(ice.ice_servers.len(), 1);
        assert_eq!(
            ice.ice_servers[0].urls,
            vec!["turn:10.79.3.2:3478?transport=udp".to_string()]
        );
        assert_eq!(ice.ice_servers[0].username, "conf");
        assert_eq!(ice.ice_servers[0].credential, "conf");
    }
}
