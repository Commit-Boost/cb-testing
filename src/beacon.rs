//! Beacon API client.
//!
//! Thin async wrapper over reqwest returning alloy beacon types directly.
//! Only implements the endpoints needed for devnet verification.

use std::time::Duration;

use alloy_primitives::B256;
use alloy_rpc_types_beacon::{
    block::BlockResponse, config::SpecResponse, genesis::GenesisResponse, header::HeaderResponse,
    node::SyncStatus, state::FinalityCheckpointsResponse,
};
use eyre::{Result, WrapErr};
use serde::Deserialize;
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimal beacon API client for devnet verification.
pub struct BeaconClient {
    client: reqwest::Client,
    base_url: String,
}

/// Wrapper for beacon API responses with `data` envelope.
#[derive(Deserialize)]
struct DataWrapper<T> {
    data: T,
}

/// Minimal block body for extracting execution_payload.block_hash.
/// We use serde_json::Value for the body since the block format varies by fork
/// and we only need block_hash.
#[derive(Deserialize)]
struct MinimalBlockBody {
    execution_payload: Option<MinimalExecutionPayload>,
}

#[derive(Deserialize)]
struct MinimalExecutionPayload {
    block_hash: B256,
}

impl BeaconClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("failed to build HTTP client"),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// GET /eth/v1/beacon/headers/head -> head slot
    pub async fn get_head_slot(&self) -> Result<u64> {
        let resp: HeaderResponse = self
            .client
            .get(format!("{}/eth/v1/beacon/headers/head", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp.data.header.message.slot)
    }

    /// GET /eth/v1/beacon/states/head/finality_checkpoints -> finalized epoch
    pub async fn get_finalized_epoch(&self) -> Result<u64> {
        let resp: FinalityCheckpointsResponse = self
            .client
            .get(format!(
                "{}/eth/v1/beacon/states/head/finality_checkpoints",
                self.base_url
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp.data.finalized.epoch)
    }

    /// GET /eth/v1/node/syncing -> is_syncing
    pub async fn is_syncing(&self) -> Result<bool> {
        let resp: DataWrapper<SyncStatus> = self
            .client
            .get(format!("{}/eth/v1/node/syncing", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp.data.is_syncing)
    }

    /// GET /eth/v1/beacon/genesis -> genesis_time
    pub async fn get_genesis_time(&self) -> Result<u64> {
        let resp: GenesisResponse = self
            .client
            .get(format!("{}/eth/v1/beacon/genesis", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp.data.genesis_time)
    }

    /// GET /eth/v1/config/spec -> SECONDS_PER_SLOT (defaults to 12)
    pub async fn get_seconds_per_slot(&self) -> u64 {
        match self.try_get_seconds_per_slot().await {
            Ok(sps) => sps,
            Err(e) => {
                warn!("Failed to get SECONDS_PER_SLOT, defaulting to 12: {e}");
                12
            }
        }
    }

    async fn try_get_seconds_per_slot(&self) -> Result<u64> {
        let resp: SpecResponse = self
            .client
            .get(format!("{}/eth/v1/config/spec", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        resp.data
            .get("SECONDS_PER_SLOT")
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| eyre::eyre!("SECONDS_PER_SLOT not found in spec"))
    }

    /// GET /eth/v1/beacon/headers/{slot} -> Some(header) or None if 404
    pub async fn get_header(&self, slot: u64) -> Result<Option<HeaderResponse>> {
        let resp = self
            .client
            .get(format!("{}/eth/v1/beacon/headers/{slot}", self.base_url))
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        let header: HeaderResponse = resp.error_for_status()?.json().await?;
        Ok(Some(header))
    }

    /// GET /eth/v2/beacon/blocks/{slot} -> execution_payload.block_hash or None if 404/missing
    pub async fn get_block_hash(&self, slot: u64) -> Result<Option<B256>> {
        let resp = self
            .client
            .get(format!("{}/eth/v2/beacon/blocks/{slot}", self.base_url))
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        // Parse as BlockResponse with our minimal body type to extract just block_hash
        let block: BlockResponse<MinimalBlockBody> = resp
            .error_for_status()?
            .json()
            .await
            .wrap_err("failed to parse block response")?;

        Ok(block
            .data
            .message
            .body
            .execution_payload
            .map(|ep| ep.block_hash))
    }

    /// GET /eth/v1/beacon/states/head/validators?status=active_ongoing
    ///
    /// Returns pubkeys (hex with 0x prefix) of currently active validators.
    /// On error, returns Err so callers can choose to SKIP.
    pub async fn get_active_validator_pubkeys(&self) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct Validator {
            pubkey: String,
        }
        #[derive(Deserialize)]
        struct Entry {
            validator: Validator,
        }

        let resp: DataWrapper<Vec<Entry>> = self
            .client
            .get(format!(
                "{}/eth/v1/beacon/states/head/validators",
                self.base_url
            ))
            .query(&[("status", "active_ongoing")])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(resp.data.into_iter().map(|e| e.validator.pubkey).collect())
    }
}
