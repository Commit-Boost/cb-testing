//! Beacon API client.
//!
//! Thin async wrapper over reqwest returning alloy beacon types directly.
//! Only implements the endpoints needed for devnet verification.

use std::time::Duration;

use alloy_primitives::B256;
use alloy_rpc_types_beacon::{
    block::BlockResponse, header::HeaderResponse, node::SyncStatus,
    state::FinalityCheckpointsResponse,
};
use eyre::{Result, WrapErr};
use serde::Deserialize;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape a real beacon node returns for `/eth/v2/beacon/blocks/{slot}`,
    /// captured live from **prysm v7.1.8** on the nethermind+prysm devnet
    /// (2026-08-04) and trimmed to the fields around the one we extract. The
    /// point is the fields we do NOT model: a block body carries ~12 more keys
    /// and grows every fork, so the parse must ignore unknown fields rather
    /// than fail. A regression here does not error loudly - it silently yields
    /// `None`, which `payload_hash_match` reports as "missed", i.e. a real
    /// mismatch would be indistinguishable from a missing block.
    fn prysm_block_json(extra_body_fields: bool) -> String {
        let extra = if extra_body_fields {
            r#""randao_reveal": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "eth1_data": {"deposit_root": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"},
               "graffiti": "0x0000000000000000000000000000000000000000000000000000000000000000", "proposer_slashings": [], "attester_slashings": [],
               "attestations": [], "deposits": [], "voluntary_exits": [],
               "sync_aggregate": {"sync_committee_bits": "0x00"},
               "bls_to_execution_changes": [], "blob_kzg_commitments": [],"#
        } else {
            ""
        };
        format!(
            r#"{{
              "version": "fulu",
              "execution_optimistic": false,
              "finalized": true,
              "data": {{
                "message": {{
                  "slot": "93",
                  "proposer_index": "42",
                  "parent_root": "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                  "state_root": "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                  "body": {{
                    {extra}
                    "execution_payload": {{
                      "parent_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
                      "fee_recipient": "0x0000000000000000000000000000000000000000",
                      "block_number": "75",
                      "gas_limit": "60000000",
                      "block_hash": "0xba639ff997222ed1521e1474ae80094ed4dccad19b5d2ac1b596e7fbe248cf1b",
                      "transactions": []
                    }}
                  }}
                }},
                "signature": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
              }}
            }}"#
        )
    }

    #[test]
    fn parses_block_hash_from_a_real_prysm_response() {
        let json = prysm_block_json(true);
        let block: BlockResponse<MinimalBlockBody> =
            serde_json::from_str(&json).expect("real prysm block must parse");
        assert_eq!(
            block
                .data
                .message
                .body
                .execution_payload
                .map(|ep| ep.block_hash),
            Some(
                "0xba639ff997222ed1521e1474ae80094ed4dccad19b5d2ac1b596e7fbe248cf1b"
                    .parse::<B256>()
                    .unwrap()
            )
        );
    }

    #[test]
    fn unknown_body_and_payload_fields_are_ignored() {
        // Same block with the sibling body fields stripped: the extraction must
        // be insensitive to which of them are present, so a new fork adding or
        // removing body fields cannot silently break hash comparison.
        let with = prysm_block_json(true);
        let without = prysm_block_json(false);
        let a: BlockResponse<MinimalBlockBody> = serde_json::from_str(&with).unwrap();
        let b: BlockResponse<MinimalBlockBody> = serde_json::from_str(&without).unwrap();
        assert_eq!(
            a.data.message.body.execution_payload.map(|e| e.block_hash),
            b.data.message.body.execution_payload.map(|e| e.block_hash),
        );
    }

    #[test]
    fn block_without_execution_payload_yields_none_not_an_error() {
        // A pre-merge / phase0 block has no execution_payload. That must be a
        // clean `None` (the caller reports "missed"), never a parse error that
        // would abort the whole slot scan.
        let json = r#"{
          "version": "phase0",
          "data": {
            "message": {
              "slot": "1", "proposer_index": "0",
              "parent_root": "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc", "state_root": "0xdddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
              "body": { "randao_reveal": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" }
            },
            "signature": "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
          }
        }"#;
        let block: BlockResponse<MinimalBlockBody> =
            serde_json::from_str(json).expect("payload-less block must still parse");
        assert!(block.data.message.body.execution_payload.is_none());
    }

    #[test]
    fn data_wrapper_unwraps_the_envelope() {
        // Every beacon endpoint we call wraps its payload in {"data": ...}.
        let w: DataWrapper<Vec<u64>> = serde_json::from_str(r#"{"data":[1,2,3]}"#).unwrap();
        assert_eq!(w.data, vec![1, 2, 3]);
    }

    #[test]
    fn base_url_trailing_slash_is_normalized() {
        // URLs are built by string concat, so a trailing slash would produce a
        // double slash and a 404 on some clients.
        let c = BeaconClient::new("http://beacon:5052/");
        assert_eq!(c.base_url, "http://beacon:5052");
        let c2 = BeaconClient::new("http://beacon:5052");
        assert_eq!(c2.base_url, "http://beacon:5052");
    }
}
