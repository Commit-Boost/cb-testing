//! `sim diff`: compare two verification reports and surface the verdict delta.
//!
//! The report is a round-trippable JSON interchange format (the `report` types
//! derive `Deserialize`), and each run records its [`Provenance`] (config hash +
//! resolved image ids). This verb answers "what changed between run A and run
//! B, and did anything regress?" — the CI shape being: bump an image, re-run,
//! `sim diff old.json new.json`, fail the pipeline if a check regressed.
//!
//! Verdict severity is `CheckStatus`'s own ordering (Fail > Warn > Pass > Skip).
//! A regression is a check whose severity INCREASED. A check dropping to `Skip`
//! (severity down) is shown but is NOT a regression — it is not a FAIL, and the
//! report's own tier-1 gate, not this diff, owns the pass/fail verdict. The
//! Pass->Skip coverage loss is still printed so a human sees it.

use std::collections::BTreeMap;
use std::path::Path;

use cb_testnet_verifier::checks::CheckStatus;
use cb_testnet_verifier::report::VerificationReport;
use serde::Serialize;

/// Direction of a single check's verdict change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Severity increased (e.g. Pass -> Fail).
    Regressed,
    /// Severity decreased (e.g. Fail -> Pass, or Pass -> Skip).
    Improved,
}

/// A check whose verdict changed between the two reports.
#[derive(Debug, Clone, Serialize)]
pub struct CheckDelta {
    pub id: String,
    pub from: CheckStatus,
    pub to: CheckStatus,
    pub direction: Direction,
}

/// A check present in only one of the two reports.
#[derive(Debug, Clone, Serialize)]
pub struct CheckPresence {
    pub id: String,
    pub status: CheckStatus,
}

/// One image role whose resolved name or id changed between the two reports.
#[derive(Debug, Clone, Serialize)]
pub struct ImageDelta {
    pub role: String,
    pub from_name: Option<String>,
    pub to_name: Option<String>,
    pub from_id: Option<String>,
    pub to_id: Option<String>,
}

/// The full structured diff of two reports.
#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub from_overall: CheckStatus,
    pub to_overall: CheckStatus,
    pub overall_regressed: bool,
    /// Checks whose verdict changed, sorted by id.
    pub changed: Vec<CheckDelta>,
    /// Checks in B but not A, sorted by id.
    pub added: Vec<CheckPresence>,
    /// Checks in A but not B, sorted by id.
    pub removed: Vec<CheckPresence>,
    /// `(from, to)` config hashes, present only when they differ.
    pub config_hash: Option<(Option<String>, Option<String>)>,
    /// Image roles whose name or id changed, sorted by role.
    pub images: Vec<ImageDelta>,
}

impl DiffReport {
    /// True iff anything got a strictly-worse verdict: the overall result
    /// regressed, or any individual check regressed. A check that merely went
    /// SKIP (coverage loss) is not counted — it is not a FAIL.
    pub fn has_regression(&self) -> bool {
        self.overall_regressed
            || self
                .changed
                .iter()
                .any(|c| c.direction == Direction::Regressed)
    }
}

/// Map each check id to its status. Later duplicates win (a report should not
/// have duplicate ids; if it does, the last is what a reader would see printed).
fn status_by_id(report: &VerificationReport) -> BTreeMap<String, CheckStatus> {
    report
        .checks
        .iter()
        .map(|c| (c.id.clone(), c.status))
        .collect()
}

/// Pure verdict-diff logic (the Law 4 test seam; `run` only does file IO).
pub fn diff_reports(a: &VerificationReport, b: &VerificationReport) -> DiffReport {
    let from = status_by_id(a);
    let to = status_by_id(b);

    let mut changed = Vec::new();
    let mut removed = Vec::new();
    for (id, &from_status) in &from {
        match to.get(id) {
            Some(&to_status) if to_status != from_status => {
                let direction = if to_status > from_status {
                    Direction::Regressed
                } else {
                    Direction::Improved
                };
                changed.push(CheckDelta {
                    id: id.clone(),
                    from: from_status,
                    to: to_status,
                    direction,
                });
            }
            Some(_) => {}
            None => removed.push(CheckPresence {
                id: id.clone(),
                status: from_status,
            }),
        }
    }

    let added: Vec<CheckPresence> = to
        .iter()
        .filter(|(id, _)| !from.contains_key(*id))
        .map(|(id, &status)| CheckPresence {
            id: id.clone(),
            status,
        })
        .collect();

    let overall_regressed = b.result > a.result;

    DiffReport {
        from_overall: a.result,
        to_overall: b.result,
        overall_regressed,
        changed,
        added,
        removed,
        config_hash: diff_config_hash(a, b),
        images: diff_images(a, b),
    }
}

/// `(from, to)` config hashes if they differ, else `None`. A `None` provenance
/// on either side yields a `None` hash on that side.
fn diff_config_hash(
    a: &VerificationReport,
    b: &VerificationReport,
) -> Option<(Option<String>, Option<String>)> {
    let ah = a.provenance.as_ref().and_then(|p| p.config_hash.clone());
    let bh = b.provenance.as_ref().and_then(|p| p.config_hash.clone());
    if ah != bh { Some((ah, bh)) } else { None }
}

/// Image roles whose configured name or resolved id changed. Roles present on
/// only one side are reported with the missing side's fields as `None`.
fn diff_images(a: &VerificationReport, b: &VerificationReport) -> Vec<ImageDelta> {
    let by_role = |r: &VerificationReport| -> BTreeMap<String, (String, Option<String>)> {
        r.provenance
            .as_ref()
            .map(|p| {
                p.images
                    .iter()
                    .map(|i| (i.role.clone(), (i.name.clone(), i.id.clone())))
                    .collect()
            })
            .unwrap_or_default()
    };
    let am = by_role(a);
    let bm = by_role(b);

    let mut roles: Vec<&String> = am.keys().chain(bm.keys()).collect();
    roles.sort();
    roles.dedup();

    let mut out = Vec::new();
    for role in roles {
        let av = am.get(role);
        let bv = bm.get(role);
        let from_name = av.map(|(n, _)| n.clone());
        let to_name = bv.map(|(n, _)| n.clone());
        let from_id = av.and_then(|(_, i)| i.clone());
        let to_id = bv.and_then(|(_, i)| i.clone());
        if from_name != to_name || from_id != to_id {
            out.push(ImageDelta {
                role: role.clone(),
                from_name,
                to_name,
                from_id,
                to_id,
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// IO wrapper
// ---------------------------------------------------------------------------

fn load(path: &Path) -> eyre::Result<VerificationReport> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("failed to read report '{}': {e}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("failed to parse report '{}' as JSON: {e}", path.display()))
}

/// Load two reports, diff them, print the result, and exit nonzero if a check
/// regressed (so the diff is usable as a CI gate). `Err` is reserved for IO /
/// parse failures; a regression is a clean nonzero exit, not an error.
pub fn run(from: &Path, to: &Path, json: bool) -> eyre::Result<()> {
    let a = load(from)?;
    let b = load(to)?;
    let diff = diff_reports(&a, &b);

    if json {
        println!("{}", serde_json::to_string_pretty(&diff)?);
    } else {
        print_pretty(&a, &b, &diff);
    }

    if diff.has_regression() {
        std::process::exit(1);
    }
    Ok(())
}

fn print_pretty(a: &VerificationReport, b: &VerificationReport, diff: &DiffReport) {
    println!(
        "Report diff: {}@{} -> {}@{}",
        a.enclave, a.timestamp, b.enclave, b.timestamp
    );
    let flag = if diff.overall_regressed {
        "  [REGRESSION]"
    } else {
        ""
    };
    println!(
        "Overall: {} -> {}{}",
        diff.from_overall, diff.to_overall, flag
    );

    if diff.changed.is_empty() && diff.added.is_empty() && diff.removed.is_empty() {
        println!("\nNo per-check verdict changes.");
    } else {
        println!("\nCheck changes:");
        for c in &diff.changed {
            let tag = match c.direction {
                Direction::Regressed => "REGRESSED",
                Direction::Improved => "improved ",
            };
            println!("  [{tag}] {}: {} -> {}", c.id, c.from, c.to);
        }
        for p in &diff.added {
            println!("  [added]     + {}: {}", p.id, p.status);
        }
        for p in &diff.removed {
            println!("  [removed]   - {}: {}", p.id, p.status);
        }
    }

    if diff.config_hash.is_some() || !diff.images.is_empty() {
        println!("\nProvenance:");
        if let Some((from, to)) = &diff.config_hash {
            let show = |h: &Option<String>| h.clone().unwrap_or_else(|| "none".to_string());
            println!("  config_hash: {} -> {}", show(from), show(to));
        }
        for img in &diff.images {
            let show = |o: &Option<String>| o.clone().unwrap_or_else(|| "none".to_string());
            println!(
                "  image[{}]: name {} -> {}, id {} -> {}",
                img.role,
                show(&img.from_name),
                show(&img.to_name),
                show(&img.from_id),
                show(&img.to_id),
            );
        }
    }

    println!();
    if diff.has_regression() {
        println!("Verdict: REGRESSION (a check got a worse verdict)");
    } else {
        println!("Verdict: no regression");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cb_testnet_verifier::checks::CheckResult;
    use cb_testnet_verifier::report::{ImageRef, Provenance};

    /// Build a report from `(id, status)` pairs + optional provenance.
    fn report(
        checks: &[(&str, CheckStatus)],
        provenance: Option<Provenance>,
    ) -> VerificationReport {
        let checks: Vec<CheckResult> = checks
            .iter()
            .map(|(id, status)| match status {
                CheckStatus::Pass => CheckResult::pass(*id, 1, "ok"),
                CheckStatus::Fail => CheckResult::fail(*id, 1, "bad"),
                CheckStatus::Warn => CheckResult::warn(*id, 1, "meh"),
                CheckStatus::Skip => CheckResult::skip(*id, 1, "n/a"),
            })
            .collect();
        // The overall result mirrors the worst check severity, matching how a
        // real report is minted (not load-bearing for these tests, which set it
        // explicitly via the pairs' worst status).
        let result = checks_worst(&checks);
        VerificationReport {
            enclave: "e".to_string(),
            timestamp: "t".to_string(),
            observation_window: None,
            result,
            checks,
            provenance,
        }
    }

    fn checks_worst(checks: &[CheckResult]) -> CheckStatus {
        checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(CheckStatus::Pass)
    }

    fn img(role: &str, name: &str, id: Option<&str>) -> ImageRef {
        ImageRef {
            role: role.to_string(),
            name: name.to_string(),
            id: id.map(str::to_string),
        }
    }

    fn prov(hash: Option<&str>, images: Vec<ImageRef>) -> Provenance {
        Provenance {
            config_path: None,
            config_hash: hash.map(str::to_string),
            images,
        }
    }

    #[test]
    fn identical_reports_have_no_changes_and_no_regression() {
        let a = report(&[("x", CheckStatus::Pass), ("y", CheckStatus::Warn)], None);
        let b = report(&[("x", CheckStatus::Pass), ("y", CheckStatus::Warn)], None);
        let d = diff_reports(&a, &b);
        assert!(d.changed.is_empty());
        assert!(d.added.is_empty());
        assert!(d.removed.is_empty());
        assert!(!d.has_regression());
    }

    #[test]
    fn pass_to_fail_is_a_regression() {
        let a = report(&[("x", CheckStatus::Pass)], None);
        let b = report(&[("x", CheckStatus::Fail)], None);
        let d = diff_reports(&a, &b);
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].direction, Direction::Regressed);
        assert!(d.has_regression());
        assert!(d.overall_regressed);
    }

    #[test]
    fn fail_to_pass_is_an_improvement_not_a_regression() {
        let a = report(&[("x", CheckStatus::Fail)], None);
        let b = report(&[("x", CheckStatus::Pass)], None);
        let d = diff_reports(&a, &b);
        assert_eq!(d.changed[0].direction, Direction::Improved);
        assert!(!d.has_regression());
    }

    #[test]
    fn pass_to_skip_is_shown_but_is_not_a_regression() {
        // Coverage loss: a check stopped running. Severity DROPPED (Skip < Pass)
        // so it is Improved-by-severity and does NOT fail the gate — the report's
        // own tier-1 gate owns pass/fail, not the diff. But it IS surfaced.
        let a = report(&[("x", CheckStatus::Pass)], None);
        let b = report(&[("x", CheckStatus::Skip)], None);
        let d = diff_reports(&a, &b);
        assert_eq!(d.changed.len(), 1, "the coverage change is still surfaced");
        assert_eq!(d.changed[0].to, CheckStatus::Skip);
        assert!(!d.has_regression(), "Skip is not a FAIL");
    }

    #[test]
    fn added_and_removed_checks_are_tracked() {
        let a = report(&[("only_a", CheckStatus::Pass)], None);
        let b = report(&[("only_b", CheckStatus::Pass)], None);
        let d = diff_reports(&a, &b);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.removed[0].id, "only_a");
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].id, "only_b");
        assert!(!d.has_regression(), "presence changes are not regressions");
    }

    #[test]
    fn config_hash_change_is_reported_only_when_it_differs() {
        let same_a = report(&[("x", CheckStatus::Pass)], Some(prov(Some("abc"), vec![])));
        let same_b = report(&[("x", CheckStatus::Pass)], Some(prov(Some("abc"), vec![])));
        assert!(diff_reports(&same_a, &same_b).config_hash.is_none());

        let a = report(&[("x", CheckStatus::Pass)], Some(prov(Some("abc"), vec![])));
        let b = report(&[("x", CheckStatus::Pass)], Some(prov(Some("def"), vec![])));
        let d = diff_reports(&a, &b);
        assert_eq!(
            d.config_hash,
            Some((Some("abc".to_string()), Some("def".to_string())))
        );
    }

    #[test]
    fn image_id_change_is_reported_name_unchanged() {
        let a = report(
            &[("x", CheckStatus::Pass)],
            Some(prov(
                None,
                vec![img("mev_boost", "cb:kurtosis", Some("sha256:aa"))],
            )),
        );
        let b = report(
            &[("x", CheckStatus::Pass)],
            Some(prov(
                None,
                vec![img("mev_boost", "cb:kurtosis", Some("sha256:bb"))],
            )),
        );
        let d = diff_reports(&a, &b);
        assert_eq!(d.images.len(), 1);
        assert_eq!(d.images[0].role, "mev_boost");
        assert_eq!(d.images[0].from_id, Some("sha256:aa".to_string()));
        assert_eq!(d.images[0].to_id, Some("sha256:bb".to_string()));
        assert_eq!(d.images[0].from_name, d.images[0].to_name, "name unchanged");
    }

    #[test]
    fn unchanged_image_is_not_reported() {
        let same = vec![img("mev_boost", "cb:kurtosis", Some("sha256:aa"))];
        let a = report(&[("x", CheckStatus::Pass)], Some(prov(None, same.clone())));
        let b = report(&[("x", CheckStatus::Pass)], Some(prov(None, same)));
        assert!(diff_reports(&a, &b).images.is_empty());
    }

    #[test]
    fn a_regressed_report_round_trips_through_json() {
        // The whole point of the Deserialize derive: a serialized report can be
        // read back and diffed. Serialize A and B, deserialize, diff.
        let a = report(&[("x", CheckStatus::Pass)], None);
        let b = report(&[("x", CheckStatus::Fail)], None);
        let a_json = serde_json::to_string(&a).unwrap();
        let b_json = serde_json::to_string(&b).unwrap();
        let a2: VerificationReport = serde_json::from_str(&a_json).unwrap();
        let b2: VerificationReport = serde_json::from_str(&b_json).unwrap();
        let d = diff_reports(&a2, &b2);
        assert!(d.has_regression());
        assert_eq!(d.changed[0].to, CheckStatus::Fail);
    }
}
