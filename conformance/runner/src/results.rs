//! Adapter result documents and the runner's final classification.
//!
//! Adapters report *raw* outcomes (`pass`, `fail`, `skip`) per test in a JSON
//! document. The runner reclassifies each raw outcome against the target's
//! manifest into a final [`Status`], which is what the matrix renders and what
//! decides the runner's exit code.

use serde::Deserialize;

/// A single adapter's result document: one target running in one environment.
#[derive(Debug, Clone, Deserialize)]
pub struct AdapterReport {
    /// Target id (must match a manifest `[target].id`).
    pub target: String,
    /// Scenario/environment the adapter ran in, e.g. `loopback`. Retained for
    /// per-scenario matrix columns added in a later phase.
    #[allow(dead_code)]
    pub environment: String,
    /// Raw per-test outcomes.
    #[serde(default)]
    pub results: Vec<RawResult>,
}

/// One raw per-test outcome as reported by an adapter (pre-policy).
#[derive(Debug, Clone, Deserialize)]
pub struct RawResult {
    /// Test id (matches the registry).
    pub test_id: String,
    /// Raw status the adapter observed.
    pub status: RawStatus,
    /// Optional failure/skip detail.
    #[serde(default)]
    pub detail: Option<String>,
}

/// The raw status vocabulary adapters emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RawStatus {
    /// The test passed.
    Pass,
    /// The test failed.
    Fail,
    /// The adapter/guest skipped the test (e.g. target lacks the surface).
    Skip,
}

/// The final status after the runner applies manifest policy. This is what the
/// matrix shows and what drives the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Passed and was expected to pass.
    Pass,
    /// Failed and was not expected to (fails the run).
    Fail,
    /// Skipped because the target's manifest declares a tag unsupported.
    SkipUnsupported,
    /// Failed, but the manifest declares it an expected-fail (does not fail).
    ExpectedFail,
    /// An expected-fail that unexpectedly passed (fails the run).
    UnexpectedPass,
    /// The registry lists this test but no adapter reported a result for it.
    Missing,
}

impl Status {
    /// Whether this final status should cause a nonzero runner exit.
    pub fn is_failure(self) -> bool {
        matches!(self, Status::Fail | Status::UnexpectedPass)
    }

    /// A short symbol for the markdown matrix cell.
    pub fn symbol(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "FAIL",
            Status::SkipUnsupported => "skip",
            Status::ExpectedFail => "xfail",
            Status::UnexpectedPass => "UNEXPECTED-PASS",
            Status::Missing => "—",
        }
    }
}
