//! Per-target capability manifests (`conformance/manifests/<target>.toml`).
//!
//! A manifest declares, for one conformance target, which test *tags* are
//! unsupported (whole groups skipped with a reason) and which individual test
//! *ids* are expected to fail (with a mandatory tracking reference). The runner
//! applies these declarations as policy when classifying raw adapter results.

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The parsed `<target>.toml` manifest document.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    /// Identity of the target this manifest describes.
    pub target: TargetSection,
    /// Tags this target cannot support; matching tests become `skip-unsupported`.
    #[serde(default)]
    pub unsupported: Vec<Unsupported>,
    /// Test ids expected to fail on this target; a pass becomes `unexpected-pass`.
    #[serde(default, rename = "expected-fail")]
    pub expected_fail: Vec<ExpectedFail>,
}

/// The `[target]` table.
#[derive(Debug, Clone, Deserialize)]
pub struct TargetSection {
    /// Target id, e.g. `wasmtime`, `jco-node`, `jco-browser`, `wasip3-guest`.
    pub id: String,
}

/// A `[[unsupported]]` entry: an unsupported tag plus a mandatory reason.
#[derive(Debug, Clone, Deserialize)]
pub struct Unsupported {
    /// Tag (from `tests.toml`) whose tests this target does not support.
    pub tag: String,
    /// Why the tag is unsupported (required; surfaced in the matrix).
    pub reason: String,
}

/// An `[[expected-fail]]` entry: a test id plus mandatory tracking reference.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedFail {
    /// Test id that is known to fail on this target.
    pub test: String,
    /// Why it fails (required). Part of the manifest schema for human review.
    #[allow(dead_code)]
    pub reason: String,
    /// Tracking reference (e.g. a TODO.md item); required to stay honest.
    pub tracking: String,
}

impl Manifest {
    /// Parse a manifest file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let manifest: Manifest = toml::from_str(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        Ok(manifest)
    }

    /// True if any `unsupported` entry names a tag the test carries.
    pub fn is_unsupported(&self, tags: &[String]) -> Option<&Unsupported> {
        self.unsupported
            .iter()
            .find(|u| tags.iter().any(|t| t == &u.tag))
    }

    /// The `expected-fail` entry for a test id, if any.
    pub fn expected_fail(&self, test_id: &str) -> Option<&ExpectedFail> {
        self.expected_fail.iter().find(|e| e.test == test_id)
    }
}
