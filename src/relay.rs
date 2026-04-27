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
    /// The relay API supports a `cursor` param (not in the standard alloy query type)
    /// which acts as the upper bound slot. We pass it as a raw query param alongside
    /// the typed query fields.
    pub async fn get_payloads_delivered(
        &self,
        start_slot: u64,
        end_slot: u64,
    ) -> Result<Vec<ProposerPayloadDelivered>> {
        let limit = end_slot.saturating_sub(start_slot) + 1;

        let resp: Vec<ProposerPayloadDelivered> = self
            .client
            .get(format!(
                "{}/relay/v1/data/bidtraces/proposer_payload_delivered",
                self.base_url
            ))
            .query(&[
                ("cursor", end_slot.to_string()),
                ("limit", limit.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        // Filter to our slot range
        Ok(resp
            .into_iter()
            .filter(|p| p.slot >= start_slot && p.slot <= end_slot)
            .collect())
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
