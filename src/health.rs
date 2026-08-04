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
    /// MEV-Boost relay data API: `proposer_payload_delivered?limit=1` always
    /// returns 200 (empty array `[]` if no payloads) -- no slot dependency,
    /// always valid.
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
                "{}/relay/v1/data/bidtraces/proposer_payload_delivered?limit=1",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_urls_match_each_service_kind() {
        // These paths are the liveness contract with three different servers.
        // A wrong path returns 404 - which `probe` counts as ALIVE (only
        // transport errors mean death), so a typo here would make the death
        // detector permanently blind rather than fail loudly.
        assert_eq!(
            HealthTarget::new("b", "http://cl:5052", ServiceKind::Beacon).probe_url(),
            "http://cl:5052/eth/v1/node/health"
        );
        assert_eq!(
            HealthTarget::new("r", "http://relay:4040", ServiceKind::Relay).probe_url(),
            "http://relay:4040/relay/v1/data/bidtraces/proposer_payload_delivered?limit=1"
        );
        assert_eq!(
            HealthTarget::new("c", "http://cb:18550", ServiceKind::CbPbs).probe_url(),
            "http://cb:18550/eth/v1/builder/status"
        );
    }

    #[test]
    fn trailing_slash_never_produces_a_double_slash() {
        // URLs are built by concatenation; `//` 404s on some servers, which
        // would again read as "alive" and blind the detector.
        let t = HealthTarget::new("b", "http://cl:5052/", ServiceKind::Beacon);
        assert_eq!(t.base_url, "http://cl:5052");
        assert!(!t.probe_url().contains("5052//"));
    }

    #[tokio::test]
    async fn probe_reports_transport_failure_as_death() {
        // Port 1 on localhost refuses instantly: the one condition that must
        // count as dead. No network dependency beyond loopback.
        let client = reqwest::Client::new();
        let t = HealthTarget::new("dead", "http://127.0.0.1:1", ServiceKind::Beacon);
        assert!(probe(&client, &t).await.is_err());
    }

    #[tokio::test]
    async fn probe_all_returns_the_labels_that_failed() {
        let client = reqwest::Client::new();
        let targets = vec![
            HealthTarget::new("dead-a", "http://127.0.0.1:1", ServiceKind::Beacon),
            HealthTarget::new("dead-b", "http://127.0.0.1:1", ServiceKind::Relay),
        ];
        let failed = probe_all(&client, &targets).await;
        assert_eq!(failed.len(), 2);
        assert!(failed.contains(&"dead-a".to_string()));
        assert!(failed.contains(&"dead-b".to_string()));
    }

    #[tokio::test]
    async fn probe_all_with_no_targets_reports_nothing_dead() {
        let client = reqwest::Client::new();
        assert!(probe_all(&client, &[]).await.is_empty());
    }
}
