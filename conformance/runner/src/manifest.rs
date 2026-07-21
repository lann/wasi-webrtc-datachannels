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
    /// Environments this entry applies to. Absent ⇒ every environment.
    /// Must be non-empty when present.
    pub environments: Option<Vec<String>>,
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
    /// Environments this entry applies to. Absent ⇒ every environment.
    /// Must be non-empty when present.
    pub environments: Option<Vec<String>>,
}

/// True if an entry's optional `environments` scope covers `environment`.
/// An absent scope covers every environment (including planning-only rows).
fn scope_applies(environments: &Option<Vec<String>>, environment: &str) -> bool {
    match environments {
        None => true,
        Some(envs) => envs.iter().any(|e| e == environment),
    }
}

impl Manifest {
    /// Parse a manifest file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading manifest {}", path.display()))?;
        let manifest: Manifest = toml::from_str(&text)
            .with_context(|| format!("parsing manifest {}", path.display()))?;
        manifest
            .validate()
            .with_context(|| format!("validating manifest {}", path.display()))?;
        Ok(manifest)
    }

    /// Structural checks beyond what serde enforces: an `environments` list,
    /// when present, must be non-empty (an empty list would match nothing and
    /// is always a mistake).
    pub fn validate(&self) -> Result<()> {
        for u in &self.unsupported {
            if matches!(&u.environments, Some(envs) if envs.is_empty()) {
                anyhow::bail!(
                    "[[unsupported]] entry for tag {:?} has an empty `environments` list; \
                     omit the key to apply to every environment",
                    u.tag
                );
            }
        }
        for e in &self.expected_fail {
            if matches!(&e.environments, Some(envs) if envs.is_empty()) {
                anyhow::bail!(
                    "[[expected-fail]] entry for test {:?} has an empty `environments` list; \
                     omit the key to apply to every environment",
                    e.test
                );
            }
        }
        Ok(())
    }

    /// The `unsupported` entry that applies to a test with these tags in this
    /// environment, if any. Environment-scoped entries take precedence over
    /// unscoped ones when both match.
    pub fn is_unsupported(&self, tags: &[String], environment: &str) -> Option<&Unsupported> {
        let matches = |u: &&Unsupported| tags.iter().any(|t| t == &u.tag);
        self.unsupported
            .iter()
            .filter(|u| scope_applies(&u.environments, environment))
            .filter(matches)
            .max_by_key(|u| u.environments.is_some())
    }

    /// The `expected-fail` entry that applies to a test id in this environment,
    /// if any. Environment-scoped entries take precedence over unscoped ones
    /// when both match.
    pub fn expected_fail(&self, test_id: &str, environment: &str) -> Option<&ExpectedFail> {
        self.expected_fail
            .iter()
            .filter(|e| scope_applies(&e.environments, environment))
            .filter(|e| e.test == test_id)
            .max_by_key(|e| e.environments.is_some())
    }
}
