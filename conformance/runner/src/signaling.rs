//! Signaling-server lifecycle for the runner.
//!
//! The runner spawns `conformance-signalingd` as a child process (one per
//! scenario), waits for it to be ready by polling `/healthz`, hands its base URL
//! to the adapters, and tears it down afterward. Keeping the server in a child
//! process (rather than in-process) matches how the runner orchestrates the
//! external adapters and keeps the runner itself synchronous and dependency
//! light.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// A running `conformance-signalingd` child process.
pub struct SignalingServer {
    child: Child,
    base_url: String,
}

impl SignalingServer {
    /// Spawn the server binary and block until `/healthz` reports ready.
    ///
    /// `bin` is the path to the built `conformance-signalingd` executable. The
    /// server binds an ephemeral localhost port and prints its base URL on
    /// stdout; this parses that line, then polls `/healthz` until ready or
    /// `timeout` elapses.
    pub fn spawn(bin: &str, timeout: Duration) -> Result<Self> {
        let mut child = Command::new(bin)
            .args(["--host", "127.0.0.1", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning signaling server `{bin}`"))?;

        let stdout = child
            .stdout
            .take()
            .context("capturing signaling server stdout")?;
        let base_url = read_base_url(stdout).context("reading signaling server URL")?;

        let server = SignalingServer { child, base_url };
        server.await_healthy(timeout)?;
        Ok(server)
    }

    /// The base URL adapters should use, e.g. `http://127.0.0.1:PORT`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Poll `GET /healthz` until it returns `200` or the timeout elapses.
    fn await_healthy(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let addr = self
            .base_url
            .strip_prefix("http://")
            .unwrap_or(&self.base_url)
            .to_string();

        loop {
            if http_healthz_ok(&addr) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("signaling server did not become healthy within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Terminate the server and reap the child.
    pub fn shutdown(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SignalingServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read the `listening on <url>` line from the server's stdout.
fn read_base_url(stdout: impl Read) -> Result<String> {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let url = line
        .rsplit_once("listening on ")
        .map(|(_, url)| url.trim().to_string())
        .filter(|u| u.starts_with("http://"))
        .with_context(|| format!("unexpected signaling server startup line: {line:?}"))?;
    Ok(url)
}

/// Minimal blocking `GET /healthz` returning true on a `200` status line. Uses a
/// raw HTTP/1.0 request so the runner needs no async HTTP dependency.
fn http_healthz_ok(host_port: &str) -> bool {
    let Ok(mut stream) = TcpStream::connect(host_port) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let req = "GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut resp = String::new();
    if stream.read_to_string(&mut resp).is_err() {
        return false;
    }
    resp.starts_with("HTTP/1.0 200") || resp.starts_with("HTTP/1.1 200")
}
