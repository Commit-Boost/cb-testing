//! Relay Data API client.
//!
//! Thin async wrapper over reqwest returning alloy relay types.
//! Implements the Flashbots relay data API endpoints needed for verification.

use std::time::Duration;

use alloy_rpc_types_beacon::relay::{BuilderBlockReceived, ProposerPayloadDelivered};
use eyre::Result;
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Relay data API client.
pub struct RelayClient {
    client: reqwest::Client,
    base_url: String,
}

impl RelayClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("failed to build HTTP client"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Lightweight liveness check against the relay data API.
    ///
    /// Any HTTP response (even 4xx) indicates the relay is reachable. Only
    /// returns Err on connection refused, DNS failure, TLS error, or timeout.
    /// Uses `proposer_payload_delivered?limit=1` which always returns 200
    /// (empty array `[]` if no payloads) regardless of slot.
    pub async fn ping(&self) -> Result<()> {
        let url = format!(
            "{}/relay/v1/data/bidtraces/proposer_payload_delivered",
            self.base_url
        );
        self.client
            .get(&url)
            .query(&[("limit", "1")])
            .send()
            .await
            .map(|_| ())
            .map_err(|e| eyre::eyre!("{e}"))
    }

    /// GET /relay/v1/data/bidtraces/proposer_payload_delivered
    ///
    /// Returns payloads delivered in the given slot range.
    ///
    /// The relay enforces a maximum limit of 200. We paginate using `cursor`
    /// (which is an opaque DB ID from the last item's `block_number` field) until
    /// we've fetched all payloads in the slot range or the relay returns no more results.
    pub async fn get_payloads_delivered(
        &self,
        start_slot: u64,
        end_slot: u64,
    ) -> Result<Vec<ProposerPayloadDelivered>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let max_pages = 50; // safety: 50 × 200 = 10,000 payloads max

        for _ in 0..max_pages {
            let url = format!(
                "{}/relay/v1/data/bidtraces/proposer_payload_delivered",
                self.base_url
            );
            let mut req = self.client.get(&url).query(&[("limit", "200")]);
            if let Some(ref c) = cursor {
                req = req.query(&[("cursor", c)]);
            }
            let resp: Vec<ProposerPayloadDelivered> = req
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;

            if resp.is_empty() {
                break;
            }

            // Check if we've gone past our slot range
            let min_slot = resp.iter().map(|p| p.slot).min().unwrap_or(0);
            let _max_slot = resp.iter().map(|p| p.slot).max().unwrap_or(0);

            // Filter to our slot range
            for p in &resp {
                if p.slot >= start_slot && p.slot <= end_slot {
                    all.push(p.clone());
                }
            }

            // If the oldest result is before our range, we can stop
            if min_slot < start_slot {
                break;
            }

            // Use the last item's block_number as cursor for pagination
            if let Some(last) = resp.last() {
                cursor = Some(last.block_number.to_string());
            } else {
                break;
            }
        }

        Ok(all)
    }

    /// GET /relay/v1/data/bidtraces/builder_blocks_received?slot={slot}
    ///
    /// The relay data API requires at least one filter param (slot, block_hash,
    /// block_number, or builder_pubkey). Limit-only queries return 400.
    pub async fn get_builder_blocks_received(
        &self,
        slot: u64,
    ) -> Result<Vec<BuilderBlockReceived>> {
        let entries: Vec<BuilderBlockReceived> = self
            .client
            .get(format!(
                "{}/relay/v1/data/bidtraces/builder_blocks_received",
                self.base_url
            ))
            .query(&[("slot", slot.to_string())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(entries)
    }

    /// Check if a validator is registered with the relay.
    ///
    /// GET /relay/v1/data/validator_registration?pubkey={pubkey}
    /// Returns true if 200, false otherwise.
    pub async fn is_validator_registered(&self, pubkey: &str) -> bool {
        match self
            .client
            .get(format!(
                "{}/relay/v1/data/validator_registration",
                self.base_url
            ))
            .query(&[("pubkey", pubkey)])
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(e) => {
                warn!("Failed to check registration for {pubkey}: {e}");
                false
            }
        }
    }
}
