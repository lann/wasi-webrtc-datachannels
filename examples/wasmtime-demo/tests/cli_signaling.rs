//! End-to-end integration test for the manual-signaling CLI demo.
//!
//! It builds the `cli-signaling` guest component (`wasm32-wasip2`), spawns two
//! instances of the `cli-signaling` host binary — an offerer and an answerer —
//! with piped stdio, and plays the human: it copies the offerer's base64 SDP
//! blob into the answerer and the answerer's blob back into the offerer, then
//! requires both processes to report the peer's greeting and exit cleanly.
//! This exercises the guest's vanilla-ICE embedding, the crate's
//! `peer-connection` host implementation, and a real `webrtc-rs` connection
//! between two separate host processes.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use base64::Engine as _;

/// How long to wait for any single expected line from a peer. Generous, since
/// ICE gathering and the wasm build both happen inside the test.
const LINE_TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn manual_signaling_round_trip() {
    let component = guest_component();

    let mut offerer = Peer::spawn(component, "offerer");
    let mut answerer = Peer::spawn(component, "answerer");

    // Relay the two vanilla-ICE SDP blobs, as a human would.
    let offer = offerer.next_blob();
    answerer.paste(&offer);
    let answer = answerer.next_blob();
    offerer.paste(&answer);

    // Both peers must report the other's greeting and exit cleanly.
    offerer.expect_line("hello from the answerer");
    answerer.expect_line("hello from the offerer");
    offerer.expect_success();
    answerer.expect_success();
}

/// One `cli-signaling` host process with piped stdio and a line-reader thread.
struct Peer {
    role: &'static str,
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
}

impl Peer {
    fn spawn(component: &'static PathBuf, role: &'static str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cli-signaling"))
            .arg(component)
            .arg(role)
            // The two peers share this machine; loopback candidates guarantee
            // a mutually reachable address.
            .env("WEBRTC_INCLUDE_LOOPBACK", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|err| panic!("spawning {role}: {err}"));
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            role,
            child,
            stdin,
            lines,
        }
    }

    /// The next single-line base64 SDP blob this peer prints.
    fn next_blob(&mut self) -> String {
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            let line = self.next_line(deadline);
            let Ok(decoded) =
                base64::engine::general_purpose::STANDARD.decode(line.trim().as_bytes())
            else {
                continue;
            };
            if decoded.starts_with(b"v=") {
                return line.trim().to_string();
            }
        }
    }

    /// Paste a blob line into the peer's stdin.
    fn paste(&mut self, blob: &str) {
        writeln!(self.stdin, "{blob}").unwrap_or_else(|err| {
            panic!("writing to {role} stdin: {err}", role = self.role);
        });
        self.stdin.flush().expect("flushing stdin");
    }

    /// Wait until the peer prints a line containing `needle`.
    fn expect_line(&mut self, needle: &str) {
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            if self.next_line(deadline).contains(needle) {
                return;
            }
        }
    }

    /// The peer's process must exit successfully.
    fn expect_success(&mut self) {
        let deadline = Instant::now() + LINE_TIMEOUT;
        loop {
            match self.child.try_wait().expect("waiting for peer") {
                Some(status) => {
                    assert!(status.success(), "{} exited with {status}", self.role);
                    return;
                }
                None if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    panic!("{} did not exit in time", self.role);
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn next_line(&mut self, deadline: Instant) -> String {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match self.lines.recv_timeout(remaining) {
            Ok(line) => line,
            Err(_) => {
                let _ = self.child.kill();
                panic!("{}: timed out waiting for output", self.role);
            }
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Build (once per test process) the `cli-signaling` guest component and return
/// its path. The `wasm32-wasip2` cdylib output is already a component.
fn guest_component() -> &'static PathBuf {
    static COMPONENT: OnceLock<PathBuf> = OnceLock::new();
    COMPONENT.get_or_init(|| {
        let guest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../cli-signaling");
        let target_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("cli-signaling-guest");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

        let mut command = Command::new(cargo);
        command
            .current_dir(&guest_dir)
            .arg("build")
            .arg("--release")
            .arg("--target")
            .arg("wasm32-wasip2")
            .arg("--target-dir")
            .arg(&target_dir);

        // The guest cross-compiles to wasm; strip env that leaks from the
        // outer `cargo test` invocation and would otherwise break the build.
        for (key, _) in std::env::vars() {
            if key.starts_with("CARGO_") || key == "RUSTFLAGS" {
                command.env_remove(key);
            }
        }

        let status = command
            .status()
            .expect("failed to spawn cargo to build the cli-signaling guest");
        assert!(
            status.success(),
            "building the cli-signaling guest failed; ensure the wasm32-wasip2 \
             target is installed"
        );

        target_dir
            .join("wasm32-wasip2")
            .join("release")
            .join("cli_signaling.wasm")
    })
}
