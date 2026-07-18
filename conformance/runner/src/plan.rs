//! Planning and classification: combine the registry, the manifests, and the
//! adapter reports into a per-(target, test) final status, and render the
//! markdown conformance matrix.

use std::collections::BTreeMap;

use crate::manifest::Manifest;
use crate::registry::Registry;
use crate::results::{AdapterReport, RawStatus, Status};

/// One classified cell of the matrix.
#[derive(Debug, Clone)]
pub struct Cell {
    pub status: Status,
    /// Reason/detail behind the status; surfaced in a detailed report by a
    /// later phase (the compact matrix shows only the status symbol).
    #[allow(dead_code)]
    pub detail: Option<String>,
}

/// The classified matrix: rows are targets, columns are tests.
pub struct Matrix {
    /// Target ids in the order their manifests were supplied.
    pub targets: Vec<String>,
    /// Test ids in registry order.
    pub tests: Vec<String>,
    /// (target, test) -> classified cell.
    pub cells: BTreeMap<(String, String), Cell>,
}

impl Matrix {
    /// Classify every (target, test) pair from the registry, the per-target
    /// manifests, and the collected adapter reports.
    ///
    /// A target with no adapter report is still planned from its manifest: its
    /// tests appear as `skip-unsupported` (where a tag is declared unsupported)
    /// or `missing` (otherwise), which is exactly the empty-target state Phase 0
    /// relies on.
    pub fn classify(
        registry: &Registry,
        manifests: &[Manifest],
        reports: &[AdapterReport],
    ) -> Self {
        let tests: Vec<String> = registry.test.iter().map(|t| t.id.clone()).collect();
        let targets: Vec<String> = manifests.iter().map(|m| m.target.id.clone()).collect();
        let mut cells = BTreeMap::new();

        for manifest in manifests {
            let target = &manifest.target.id;
            // Merge all raw results reported for this target (across environments).
            let mut raw: BTreeMap<&str, (RawStatus, Option<String>)> = BTreeMap::new();
            for report in reports.iter().filter(|r| &r.target == target) {
                for result in &report.results {
                    raw.insert(
                        result.test_id.as_str(),
                        (result.status, result.detail.clone()),
                    );
                }
            }

            for entry in &registry.test {
                let cell =
                    classify_one(manifest, &entry.id, &entry.tags, raw.get(entry.id.as_str()));
                cells.insert((target.clone(), entry.id.clone()), cell);
            }
        }

        Matrix {
            targets,
            tests,
            cells,
        }
    }

    /// True if any classified cell is a failure (fail or unexpected-pass).
    pub fn has_failures(&self) -> bool {
        self.cells.values().any(|c| c.status.is_failure())
    }

    /// Render the matrix as a markdown table (target rows × test columns).
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Conformance matrix\n\n");

        if self.targets.is_empty() {
            out.push_str("_No targets enabled._\n");
            return out;
        }
        if self.tests.is_empty() {
            out.push_str("_No tests registered._\n");
            return out;
        }

        out.push_str("| target |");
        for test in &self.tests {
            out.push_str(&format!(" {test} |"));
        }
        out.push('\n');
        out.push_str("| --- |");
        for _ in &self.tests {
            out.push_str(" --- |");
        }
        out.push('\n');

        for target in &self.targets {
            out.push_str(&format!("| {target} |"));
            for test in &self.tests {
                let symbol = self
                    .cells
                    .get(&(target.clone(), test.clone()))
                    .map(|c| c.status.symbol())
                    .unwrap_or("—");
                out.push_str(&format!(" {symbol} |"));
            }
            out.push('\n');
        }

        out.push_str("\nLegend: pass · FAIL · skip (unsupported) · xfail (expected-fail) · UNEXPECTED-PASS · — (missing/not run)\n");
        out
    }
}

fn classify_one(
    manifest: &Manifest,
    test_id: &str,
    tags: &[String],
    raw: Option<&(RawStatus, Option<String>)>,
) -> Cell {
    // Unsupported tags win regardless of whether a result was reported.
    if let Some(unsupported) = manifest.is_unsupported(tags) {
        return Cell {
            status: Status::SkipUnsupported,
            detail: Some(unsupported.reason.clone()),
        };
    }

    let expected_fail = manifest.expected_fail(test_id);

    match raw {
        None => Cell {
            status: Status::Missing,
            detail: None,
        },
        Some((RawStatus::Skip, detail)) => Cell {
            status: Status::SkipUnsupported,
            detail: detail.clone(),
        },
        Some((RawStatus::Pass, detail)) => {
            if let Some(xf) = expected_fail {
                Cell {
                    status: Status::UnexpectedPass,
                    detail: Some(format!(
                        "expected-fail passed (tracking {}); update the manifest",
                        xf.tracking
                    )),
                }
            } else {
                Cell {
                    status: Status::Pass,
                    detail: detail.clone(),
                }
            }
        }
        Some((RawStatus::Fail, detail)) => {
            if expected_fail.is_some() {
                Cell {
                    status: Status::ExpectedFail,
                    detail: detail.clone(),
                }
            } else {
                Cell {
                    status: Status::Fail,
                    detail: detail.clone(),
                }
            }
        }
    }
}
