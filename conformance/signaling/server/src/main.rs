//! `conformance-signalingd`: the conformance suite's HTTP mailbox signaling
//! server. Binds a (by default ephemeral, localhost) port and serves the
//! protocol in `conformance/signaling/PROTOCOL.md` until terminated.
//!
//! Test-only: in-memory state, no auth, per-room TTL and request-size caps.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use conformance_signalingd::state::Limits;
use conformance_signalingd::{spawn, Config};

/// Run the conformance signaling server.
#[derive(Debug, Parser)]
#[command(name = "conformance-signalingd", version)]
struct Cli {
    /// Address to bind. Default binds an ephemeral port on localhost.
    #[arg(long, default_value = "127.0.0.1")]
    host: IpAddr,

    /// Port to bind. 0 selects an ephemeral port (printed on startup).
    #[arg(long, default_value_t = 0)]
    port: u16,

    /// Default long-poll timeout in milliseconds.
    #[arg(long, default_value_t = 25_000)]
    long_poll_ms: u64,

    /// Maximum publish body size in bytes.
    #[arg(long, default_value_t = 262_144)]
    max_blob_bytes: usize,

    /// Room TTL in seconds (evicted after this much inactivity).
    #[arg(long, default_value_t = 300)]
    room_ttl_secs: u64,

    /// Maximum live rooms (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    max_rooms: usize,

    /// Maximum blobs per mailbox (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    max_blobs_per_mailbox: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = Config {
        long_poll: Duration::from_millis(cli.long_poll_ms),
        max_blob_bytes: cli.max_blob_bytes,
        limits: Limits {
            room_ttl: Duration::from_secs(cli.room_ttl_secs),
            max_rooms: cli.max_rooms,
            max_blobs_per_mailbox: cli.max_blobs_per_mailbox,
        },
        ..Config::default()
    };

    let addr = SocketAddr::new(cli.host, cli.port);
    let server = spawn(addr, config).await?;

    // The runner parses this line to learn the ephemeral URL; keep it stable.
    println!("conformance-signalingd listening on {}", server.base_url());

    tokio::signal::ctrl_c().await?;
    server.shutdown().await;
    Ok(())
}
