//! Conformance suite runner.
//!
//! Phase 0 scope: read the test registry (`tests.toml`) and any per-target
//! manifests (`manifests/*.toml`), aggregate stub adapter JSON result documents,
//! apply the expected-fail / unexpected-pass policy, render the markdown
//! conformance matrix, and exit nonzero on any `fail` or `unexpected-pass`.
//!
//! Later phases add scenario provisioning, signaling-server lifecycle, and the
//! adapter invocations that produce the result documents this runner consumes.

mod manifest;
mod plan;
mod registry;
mod results;
mod signaling;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

use manifest::Manifest;
use plan::Matrix;
use registry::Registry;
use results::AdapterReport;
use signaling::SignalingServer;

/// Aggregate conformance results and render the matrix.
#[derive(Debug, Parser)]
#[command(name = "conformance-runner", version)]
struct Cli {
    /// Path to the test registry (`tests.toml`).
    #[arg(long, default_value = "conformance/tests.toml")]
    tests: PathBuf,

    /// Directory of per-target manifests (`<target>.toml`).
    #[arg(long, default_value = "conformance/manifests")]
    manifests: PathBuf,

    /// Directory of adapter JSON result documents (`*.json`). Optional; when
    /// absent or empty, targets are planned purely from their manifests, which
    /// is the Phase 0 "no targets enabled" state.
    #[arg(long)]
    results: Option<PathBuf>,

    /// Where to write the rendered markdown matrix. Defaults to stdout when
    /// omitted.
    #[arg(long)]
    matrix_out: Option<PathBuf>,

    /// Path to the `conformance-signalingd` binary. When provided, the runner
    /// starts a signaling server (ephemeral localhost port), waits for
    /// `/healthz`, and tears it down after the run. Adapters (added in later
    /// phases) receive its base URL. With no targets enabled this simply
    /// exercises spawn/health/teardown.
    #[arg(long)]
    signaling_bin: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let registry = Registry::load(&cli.tests)?;
    let manifests = load_manifests(&cli.manifests)?;

    // Start the signaling server if requested, so it is up before adapters run.
    let signaling = match &cli.signaling_bin {
        Some(bin) => {
            let server = SignalingServer::spawn(bin, Duration::from_secs(10))
                .context("starting signaling server")?;
            eprintln!("signaling server ready at {}", server.base_url());
            Some(server)
        }
        None => None,
    };

    let reports = match &cli.results {
        Some(dir) => load_reports(dir)?,
        None => Vec::new(),
    };

    let matrix = Matrix::classify(&registry, &manifests, &reports);
    let markdown = matrix.render_markdown();

    // Adapters run between server startup and teardown in later phases; for now
    // tear the server down once the (stub) aggregation is complete.
    if let Some(server) = signaling {
        server.shutdown();
    }

    match &cli.matrix_out {
        Some(path) => {
            std::fs::write(path, &markdown)
                .with_context(|| format!("writing matrix to {}", path.display()))?;
            eprintln!("wrote conformance matrix to {}", path.display());
        }
        None => print!("{markdown}"),
    }

    if matrix.has_failures() {
        eprintln!("\nconformance failures:");
        for (row, test, cell) in matrix.failures() {
            let target = if row.environment.is_empty() {
                row.target.clone()
            } else {
                format!("{} [{}]", row.target, row.environment)
            };
            match &cell.detail {
                Some(detail) => {
                    eprintln!("  {target} / {test}: {} — {detail}", cell.status.symbol())
                }
                None => eprintln!("  {target} / {test}: {}", cell.status.symbol()),
            }
        }
        anyhow::bail!("conformance run has failing or unexpected-pass results");
    }

    eprintln!(
        "conformance run OK: {} row(s), {} test(s), no failures",
        matrix.rows.len(),
        matrix.tests.len()
    );
    Ok(())
}

/// Load every `*.toml` manifest from a directory (missing dir => no targets).
fn load_manifests(dir: &std::path::Path) -> Result<Vec<Manifest>> {
    let mut manifests = Vec::new();
    if !dir.exists() {
        return Ok(manifests);
    }
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading manifests dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    paths.sort();
    for path in paths {
        manifests.push(Manifest::load(&path)?);
    }
    Ok(manifests)
}

/// Load every `*.json` adapter report from a directory (missing dir => none).
fn load_reports(dir: &std::path::Path) -> Result<Vec<AdapterReport>> {
    let mut reports = Vec::new();
    if !dir.exists() {
        return Ok(reports);
    }
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading results dir {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading result {}", path.display()))?;
        let report: AdapterReport = serde_json::from_str(&text)
            .with_context(|| format!("parsing result {}", path.display()))?;
        reports.push(report);
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{RawResult, RawStatus, Status};

    fn registry() -> Registry {
        toml::from_str(
            r#"
            [[test]]
            id = "ordering"
            tags = ["data-channel"]
            description = "ordered payloads arrive in order"

            [[test]]
            id = "error-invalid-signaling"
            tags = ["errors"]
            description = "invalid SDP yields invalid-signaling"

            [[test]]
            id = "peer-offer-answer"
            tags = ["peer-connection"]
            description = "offer/answer happy path"
            "#,
        )
        .unwrap()
    }

    fn manifest() -> Manifest {
        toml::from_str(
            r#"
            [target]
            id = "wasmtime"

            [[unsupported]]
            tag = "peer-connection"
            reason = "host does not implement peer-connection yet"

            [[expected-fail]]
            test = "error-invalid-signaling"
            reason = "collapses to error.other"
            tracking = "TODO.md item 5"
            "#,
        )
        .unwrap()
    }

    fn report(results: Vec<(&str, RawStatus)>) -> AdapterReport {
        AdapterReport {
            target: "wasmtime".into(),
            environment: "loopback".into(),
            results: results
                .into_iter()
                .map(|(id, status)| RawResult {
                    test_id: id.into(),
                    status,
                    detail: None,
                })
                .collect(),
        }
    }

    fn status_of(matrix: &Matrix, test: &str) -> Status {
        matrix
            .cells
            .get(&(
                "wasmtime".to_string(),
                "loopback".to_string(),
                test.to_string(),
            ))
            .unwrap()
            .status
    }

    #[test]
    fn empty_run_has_no_failures() {
        let matrix = Matrix::classify(&registry(), &[], &[]);
        assert!(!matrix.has_failures());
        assert!(matrix.rows.is_empty());
    }

    #[test]
    fn unsupported_tag_skips_regardless_of_result() {
        let m = manifest();
        let reports = [report(vec![("peer-offer-answer", RawStatus::Fail)])];
        let matrix = Matrix::classify(&registry(), &[m], &reports);
        assert_eq!(
            status_of(&matrix, "peer-offer-answer"),
            Status::SkipUnsupported
        );
        assert!(!matrix.has_failures());
    }

    #[test]
    fn expected_fail_that_fails_is_not_a_failure() {
        let m = manifest();
        let reports = [report(vec![("error-invalid-signaling", RawStatus::Fail)])];
        let matrix = Matrix::classify(&registry(), &[m], &reports);
        assert_eq!(
            status_of(&matrix, "error-invalid-signaling"),
            Status::ExpectedFail
        );
        assert!(!matrix.has_failures());
    }

    #[test]
    fn expected_fail_that_passes_is_unexpected_pass_and_fails() {
        let m = manifest();
        let reports = [report(vec![("error-invalid-signaling", RawStatus::Pass)])];
        let matrix = Matrix::classify(&registry(), &[m], &reports);
        assert_eq!(
            status_of(&matrix, "error-invalid-signaling"),
            Status::UnexpectedPass
        );
        assert!(matrix.has_failures());
    }

    #[test]
    fn plain_fail_fails_the_run() {
        let m = manifest();
        let reports = [report(vec![("ordering", RawStatus::Fail)])];
        let matrix = Matrix::classify(&registry(), &[m], &reports);
        assert_eq!(status_of(&matrix, "ordering"), Status::Fail);
        assert!(matrix.has_failures());
    }

    #[test]
    fn pass_passes_and_missing_is_missing() {
        let m = manifest();
        let reports = [report(vec![("ordering", RawStatus::Pass)])];
        let matrix = Matrix::classify(&registry(), &[m], &reports);
        assert_eq!(status_of(&matrix, "ordering"), Status::Pass);
        // error-invalid-signaling had no result reported -> missing (but it is an
        // expected-fail, so still not a run failure since missing is neutral).
        assert_eq!(
            status_of(&matrix, "error-invalid-signaling"),
            Status::Missing
        );
        assert!(!matrix.has_failures());
    }

    fn report_env(environment: &str, results: Vec<(&str, RawStatus)>) -> AdapterReport {
        AdapterReport {
            target: "wasmtime".into(),
            environment: environment.into(),
            results: results
                .into_iter()
                .map(|(id, status)| RawResult {
                    test_id: id.into(),
                    status,
                    detail: None,
                })
                .collect(),
        }
    }

    fn status_in(matrix: &Matrix, environment: &str, test: &str) -> Status {
        matrix
            .cells
            .get(&(
                "wasmtime".to_string(),
                environment.to_string(),
                test.to_string(),
            ))
            .unwrap()
            .status
    }

    #[test]
    fn environments_are_independent_rows() {
        // The same target running in two environments yields two rows, each
        // classified against its own raw results rather than a merge of them.
        let reports = [
            report_env("loopback", vec![("ordering", RawStatus::Pass)]),
            report_env("lan", vec![("ordering", RawStatus::Fail)]),
        ];
        let matrix = Matrix::classify(&registry(), &[manifest()], &reports);
        assert_eq!(matrix.rows.len(), 2);
        assert_eq!(status_in(&matrix, "loopback", "ordering"), Status::Pass);
        assert_eq!(status_in(&matrix, "lan", "ordering"), Status::Fail);
        assert!(matrix.has_failures());
    }

    #[test]
    fn manifest_only_target_is_a_planning_row() {
        // With no adapter report, a target still yields one planning-only row
        // (empty environment) so its manifest policy is visible in the matrix.
        let matrix = Matrix::classify(&registry(), &[manifest()], &[]);
        assert_eq!(matrix.rows.len(), 1);
        assert!(matrix.rows[0].environment.is_empty());
        assert_eq!(status_in(&matrix, "", "ordering"), Status::Missing);
        assert_eq!(
            status_in(&matrix, "", "peer-offer-answer"),
            Status::SkipUnsupported
        );
        assert!(!matrix.has_failures());
    }

    fn scoped_manifest() -> Manifest {
        toml::from_str(
            r#"
            [target]
            id = "wasmtime"

            [[unsupported]]
            tag = "peer-connection"
            reason = "no peer-connection under shadow"
            environments = ["shadow"]

            [[expected-fail]]
            test = "error-invalid-signaling"
            reason = "relay path collapses the error"
            tracking = "TODO.md item 9"
            environments = ["turn-relay"]
            "#,
        )
        .unwrap()
    }

    #[test]
    fn unscoped_entries_apply_in_every_environment() {
        let m = manifest();
        let reports = [
            report_env(
                "loopback",
                vec![("error-invalid-signaling", RawStatus::Fail)],
            ),
            report_env("shadow", vec![("error-invalid-signaling", RawStatus::Fail)]),
        ];
        let matrix = Matrix::classify(&registry(), &[m], &reports);
        assert_eq!(
            status_in(&matrix, "loopback", "error-invalid-signaling"),
            Status::ExpectedFail
        );
        assert_eq!(
            status_in(&matrix, "shadow", "error-invalid-signaling"),
            Status::ExpectedFail
        );
        assert!(!matrix.has_failures());
    }

    #[test]
    fn scoped_expected_fail_applies_only_in_its_environment() {
        let m = scoped_manifest();
        // Fail: xfail in turn-relay, a real FAIL elsewhere.
        let reports = [
            report_env(
                "turn-relay",
                vec![("error-invalid-signaling", RawStatus::Fail)],
            ),
            report_env(
                "loopback",
                vec![("error-invalid-signaling", RawStatus::Fail)],
            ),
        ];
        let matrix = Matrix::classify(&registry(), std::slice::from_ref(&m), &reports);
        assert_eq!(
            status_in(&matrix, "turn-relay", "error-invalid-signaling"),
            Status::ExpectedFail
        );
        assert_eq!(
            status_in(&matrix, "loopback", "error-invalid-signaling"),
            Status::Fail
        );
        assert!(matrix.has_failures());

        // Pass: UNEXPECTED-PASS in turn-relay, plain pass elsewhere.
        let reports = [
            report_env(
                "turn-relay",
                vec![("error-invalid-signaling", RawStatus::Pass)],
            ),
            report_env(
                "loopback",
                vec![("error-invalid-signaling", RawStatus::Pass)],
            ),
        ];
        let matrix = Matrix::classify(&registry(), &[m], &reports);
        assert_eq!(
            status_in(&matrix, "turn-relay", "error-invalid-signaling"),
            Status::UnexpectedPass
        );
        assert_eq!(
            status_in(&matrix, "loopback", "error-invalid-signaling"),
            Status::Pass
        );
        assert!(matrix.has_failures());
    }

    #[test]
    fn scoped_unsupported_skips_only_in_its_environment() {
        let m = scoped_manifest();
        let reports = [
            report_env("shadow", vec![("peer-offer-answer", RawStatus::Fail)]),
            report_env("loopback", vec![("peer-offer-answer", RawStatus::Fail)]),
        ];
        let matrix = Matrix::classify(&registry(), &[m], &reports);
        assert_eq!(
            status_in(&matrix, "shadow", "peer-offer-answer"),
            Status::SkipUnsupported
        );
        assert_eq!(
            status_in(&matrix, "loopback", "peer-offer-answer"),
            Status::Fail
        );
    }

    #[test]
    fn empty_environments_list_is_rejected() {
        let m: Manifest = toml::from_str(
            r#"
            [target]
            id = "wasmtime"

            [[expected-fail]]
            test = "ordering"
            reason = "r"
            tracking = "t"
            environments = []
            "#,
        )
        .unwrap();
        assert!(m.validate().is_err());

        let m: Manifest = toml::from_str(
            r#"
            [target]
            id = "wasmtime"

            [[unsupported]]
            tag = "errors"
            reason = "r"
            environments = []
            "#,
        )
        .unwrap();
        assert!(m.validate().is_err());
    }

    #[test]
    fn existing_manifest_files_still_load() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../manifests");
        let mut loaded = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) == Some("toml") {
                Manifest::load(&path)
                    .unwrap_or_else(|e| panic!("loading {}: {e:#}", path.display()));
                loaded += 1;
            }
        }
        assert!(loaded > 0, "no manifests found in {}", dir.display());
    }
}
