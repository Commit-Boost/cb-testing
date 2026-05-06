//! Verification check infrastructure: result types and status enum.

use serde::Serialize;

/// Status of a single verification check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

/// Result of a single verification check.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub id: String,
    pub tier: u8,
    #[serde(rename = "result")]
    pub status: CheckStatus,
    pub detail: String,
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

pub mod cb_metrics;
pub mod chain_health;
pub mod mux_routing;
pub mod payload_matching;
pub mod relay_pipeline;
