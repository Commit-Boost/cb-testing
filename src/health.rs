//! Liveness probing for enclave services.
//!
//! Kurtosis doesn't mark a container as down when it exits -- the service
//! object sticks around with the same port mappings, but HTTP requests get
//! TCP-reset (or connection-refused once iptables catches up). This module
//! provides lightweight ping checks so the verifier can notice mid-run
//! instead of waiting out the entire observation window.
//!
//! Every target exposes a URL known to return *some* HTTP response when the
//! service is up. 4xx/5xx are considered "alive" -- only transport errors
//! (connection refused, timeout, TCP reset) count as death.

use std::time::Duration;

use eyre::{Result, eyre};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What kind of service this target is. Determines which liveness URL to hit.
#[derive(Debug, Clone, Copy)]
pub enum ServiceKind {
    /// Beacon API: `/eth/v1/node/health` returns 200 when ready, 206 when
    /// syncing -- both prove the service is alive.
    Beacon,
    /// MEV-Boost relay data API: `builder_blocks_received?slot=0` returns
    /// 200 (empty list) or 400 (if slot is invalid for the fork) -- either
    /// way, the service answered.
    Relay,
    /// Commit-Boost PBS: `/eth/v1/builder/status` is the standard
    /// mev-boost liveness endpoint, returns 200 when running.
    CbPbs,
}

/// A service to monitor during the observation window.
#[derive(Debug, Clone)]
pub struct HealthTarget {
    /// Human-readable label used in logs ("beacon cl-1", "relay", "cb-pbs").
    pub label: String,
    /// Base URL (no trailing slash). The probe appends the kind's path.
    pub base_url: String,
    pub kind: ServiceKind,
}

impl HealthTarget {
    pub fn new(label: impl Into<String>, base_url: impl Into<String>, kind: ServiceKind) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        Self {
            label: label.into(),
            base_url: base,
            kind,
        }
    }

    /// Build the full URL to hit for a liveness probe.
    fn probe_url(&self) -> String {
        match self.kind {
            ServiceKind::Beacon => format!("{}/eth/v1/node/health", self.base_url),
            ServiceKind::Relay => format!(
                "{}/relay/v1/data/bidtraces/builder_blocks_received?slot=0",
                self.base_url
            ),
            ServiceKind::CbPbs => format!("{}/eth/v1/builder/status", self.base_url),
        }
    }
}

/// Probe a single target. Returns Ok iff *any* HTTP response came back.
pub async fn probe(client: &reqwest::Client, target: &HealthTarget) -> Result<()> {
    client
        .get(target.probe_url())
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .map(|_| ())
        .map_err(|e| eyre!("{e}"))
}

/// Probe every target. Returns the list of labels that failed.
///
/// Runs sequentially -- a handful of targets per enclave, not worth the
/// complexity of parallel futures.
pub async fn probe_all(
    client: &reqwest::Client,
    targets: &[HealthTarget],
) -> Vec<(String, eyre::Error)> {
    let mut dead = Vec::new();
    for t in targets {
        if let Err(e) = probe(client, t).await {
            dead.push((t.label.clone(), e));
        }
    }
    dead
}
