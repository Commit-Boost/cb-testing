//! Verification check infrastructure: result types and status enum.

use serde::{Deserialize, Serialize};

/// Status of a single verification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CheckStatus {
    Pass,
    Fail,
    Warn,
    Skip,
}

impl std::fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::Fail => write!(f, "FAIL"),
            Self::Warn => write!(f, "WARN"),
            Self::Skip => write!(f, "SKIP"),
        }
    }
}

impl CheckStatus {
    /// Severity rank for worst-status aggregation: `Fail > Warn > Pass > Skip`.
    ///
    /// This is the order the hand-rolled worst-status folds in
    /// `relay_pipeline::run_relay_checks` implied (a `Fail` from any relay must
    /// win the aggregate; `Skip` is the least severe). Deriving `Ord` would use
    /// declaration order (`Pass, Fail, Warn, Skip`) which is NOT this order, so
    /// the rank is spelled out explicitly.
    fn severity(self) -> u8 {
        match self {
            Self::Skip => 0,
            Self::Pass => 1,
            Self::Warn => 2,
            Self::Fail => 3,
        }
    }
}

impl Ord for CheckStatus {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.severity().cmp(&other.severity())
    }
}

impl PartialOrd for CheckStatus {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Result of a single verification check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub id: String,
    pub tier: u8,
    #[serde(rename = "result")]
    pub status: CheckStatus,
    pub detail: String,
    #[serde(default = "empty_data")]
    pub data: serde_json::Value,
}

fn empty_data() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

impl CheckResult {
    pub fn pass(id: impl Into<String>, tier: u8, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            tier,
            status: CheckStatus::Pass,
            detail: detail.into(),
            data: empty_data(),
        }
    }

    pub fn fail(id: impl Into<String>, tier: u8, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            tier,
            status: CheckStatus::Fail,
            detail: detail.into(),
            data: empty_data(),
        }
    }

    pub fn warn(id: impl Into<String>, tier: u8, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            tier,
            status: CheckStatus::Warn,
            detail: detail.into(),
            data: empty_data(),
        }
    }

    pub fn skip(id: impl Into<String>, tier: u8, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            tier,
            status: CheckStatus::Skip,
            detail: detail.into(),
            data: empty_data(),
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }
}

pub mod best_bid;
pub mod cb_metrics;
pub mod chain_health;
pub mod feature_fired;
pub mod mux_routing;
pub mod payload_matching;
pub mod relay_pipeline;

#[cfg(test)]
mod status_ord_tests {
    use super::CheckStatus;

    // Contract: the worst-status ordering is Fail > Warn > Pass > Skip. This is
    // the rank the hand-rolled folds in run_relay_checks aggregate by, so
    // `.max()` over an iterator of statuses reproduces "the worst wins".
    #[test]
    fn severity_order_is_fail_warn_pass_skip() {
        assert!(CheckStatus::Fail > CheckStatus::Warn);
        assert!(CheckStatus::Warn > CheckStatus::Pass);
        assert!(CheckStatus::Pass > CheckStatus::Skip);
        // Transitively, Fail is the maximum and Skip the minimum.
        assert!(CheckStatus::Fail > CheckStatus::Skip);
    }

    // Contract: Fail beats Warn beats Pass when aggregating a mixed set.
    #[test]
    fn max_picks_fail_over_warn_over_pass() {
        let statuses = [CheckStatus::Pass, CheckStatus::Warn, CheckStatus::Fail];
        assert_eq!(statuses.into_iter().max(), Some(CheckStatus::Fail));

        let no_fail = [CheckStatus::Pass, CheckStatus::Warn, CheckStatus::Pass];
        assert_eq!(no_fail.into_iter().max(), Some(CheckStatus::Warn));

        let all_pass = [CheckStatus::Pass, CheckStatus::Pass];
        assert_eq!(all_pass.into_iter().max(), Some(CheckStatus::Pass));
    }

    // Contract: Skip is the least severe, so a Skip mixed with any real verdict
    // never wins the aggregate (mirrors the registrations fold: Skip+Pass=Pass).
    #[test]
    fn skip_is_least_severe() {
        assert_eq!(
            [CheckStatus::Skip, CheckStatus::Pass].into_iter().max(),
            Some(CheckStatus::Pass)
        );
        assert_eq!(
            [CheckStatus::Skip, CheckStatus::Warn].into_iter().max(),
            Some(CheckStatus::Warn)
        );
        // An all-Skip set aggregates to Skip (this input is unreachable in the
        // registrations fold, which is why that fold's old init-Pass and this
        // rule differ only on the impossible case — see the fold's comment).
        assert_eq!(
            [CheckStatus::Skip, CheckStatus::Skip].into_iter().max(),
            Some(CheckStatus::Skip)
        );
    }

    // Contract: an empty iterator yields None; callers pick their own default
    // (run_relay_checks guards non-emptiness before aggregating).
    #[test]
    fn empty_iter_max_is_none() {
        let empty: [CheckStatus; 0] = [];
        assert_eq!(empty.into_iter().max(), None);
    }
}
