//! Relay Data API client.
//!
//! Thin async wrapper over reqwest returning alloy relay types.
//! Implements the helix relay data API endpoints needed for verification
//! (the relays run `ghcr.io/gattaca-com/helix-relay`, not the Flashbots
//! reference relay, so helix's response contract is what we target here).

use std::collections::HashSet;
use std::time::Duration;

use alloy_primitives::B256;
use alloy_rpc_types_beacon::relay::{BuilderBlockReceived, ProposerPayloadDelivered};
use eyre::Result;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The relay caps `limit` at 200 rows per page.
const PAGE_LIMIT: usize = 200;

/// Safety bound on pagination: 50 × 200 = 10,000 payloads max.
const MAX_PAGES: usize = 50;

/// Decide whether to keep paging after fetching a page.
///
/// This is the pure loop-termination seam for `get_payloads_delivered`. It is
/// deliberately order-agnostic (works whether the relay returns rows ascending
/// or descending by slot) and defensive against a relay that ignores the
/// `cursor` query param.
///
/// Returns `true` to keep paging, `false` to stop.
///
/// - **No-progress / ignored-cursor guard:** if the next cursor equals the
///   previous one, the relay did not advance the page (e.g. helix ignored the
///   cursor and re-served the same 200 rows). Stop rather than refetch the
///   same page up to `MAX_PAGES` times.
/// - **Range-complete guard:** once we have collected at least one in-range row
///   (`prev_seen_count > 0`), a page that adds no new in-range rows means we
///   have paged past the requested window in whichever direction the relay
///   orders results. Stop. While `prev_seen_count == 0` we keep paging so that
///   an ascending relay whose earliest pages sit below the window still reaches
///   the window instead of terminating after page 1.
fn page_made_progress(
    prev_cursor: Option<&str>,
    new_cursor: Option<&str>,
    prev_seen_count: usize,
    new_seen_count: usize,
) -> bool {
    if new_cursor == prev_cursor {
        return false;
    }
    if prev_seen_count > 0 && new_seen_count == prev_seen_count {
        return false;
    }
    true
}

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
    /// The relay caps `limit` at 200, so we paginate with `cursor` (an opaque
    /// DB id we source from the last row's `block_number`). We make **no**
    /// assumption about how helix orders results (ascending or descending by
    /// slot) or whether it honors `cursor` at all:
    ///
    /// - Termination is order-agnostic: we stop once a page adds no new
    ///   in-range rows after we have already collected some (see
    ///   [`page_made_progress`]), which terminates correctly in either
    ///   direction without undercounting deliveries past the first page.
    /// - If the relay ignores `cursor` and re-serves the same page, the cursor
    ///   does not advance and we stop after 2 pages instead of refetching the
    ///   same rows `MAX_PAGES` times.
    /// - Rows are de-duplicated by `block_hash`, so a refetched page cannot
    ///   double-count a delivery.
    pub async fn get_payloads_delivered(
        &self,
        start_slot: u64,
        end_slot: u64,
    ) -> Result<Vec<ProposerPayloadDelivered>> {
        let mut all = Vec::new();
        let mut seen: HashSet<B256> = HashSet::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let url = format!(
                "{}/relay/v1/data/bidtraces/proposer_payload_delivered",
                self.base_url
            );
            let mut req = self
                .client
                .get(&url)
                .query(&[("limit", PAGE_LIMIT.to_string())]);
            if let Some(ref c) = cursor {
                req = req.query(&[("cursor", c)]);
            }
            let resp: Vec<ProposerPayloadDelivered> =
                req.send().await?.error_for_status()?.json().await?;

            if resp.is_empty() {
                break;
            }

            let prev_seen_count = seen.len();

            // Collect in-range rows, de-duplicated by block hash so a relay that
            // ignores the cursor (and re-serves the same page) cannot
            // double-count a delivery.
            for p in &resp {
                if p.slot >= start_slot && p.slot <= end_slot && seen.insert(p.block_hash) {
                    all.push(p.clone());
                }
            }
            let new_seen_count = seen.len();

            // A short page means the relay had nothing more for this query; this
            // is the only stop condition the normal small-window case hits, so
            // it still completes in a single request.
            if resp.len() < PAGE_LIMIT {
                break;
            }

            let new_cursor = resp.last().map(|last| last.block_number.to_string());
            if !page_made_progress(
                cursor.as_deref(),
                new_cursor.as_deref(),
                prev_seen_count,
                new_seen_count,
            ) {
                break;
            }
            cursor = new_cursor;
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
}

#[cfg(test)]
mod tests {
    use super::page_made_progress;

    #[test]
    fn continues_when_cursor_advances_and_coverage_grows() {
        // Normal forward progress: new cursor, new in-range rows.
        assert!(page_made_progress(Some("100"), Some("101"), 5, 12));
    }

    #[test]
    fn continues_on_first_page() {
        // Page 1: no previous cursor, first in-range rows collected.
        assert!(page_made_progress(None, Some("42"), 0, 10));
    }

    #[test]
    fn stops_when_cursor_does_not_advance() {
        // Relay ignored the cursor and re-served the same page (same last
        // block_number). Bounds the ignore-cursor case to 2 pages.
        assert!(!page_made_progress(Some("77"), Some("77"), 3, 3));
        // ...even if it somehow reported "new" rows, an unchanged cursor stops us.
        assert!(!page_made_progress(Some("77"), Some("77"), 3, 9));
    }

    #[test]
    fn stops_when_in_range_and_page_adds_nothing_new() {
        // We already collected in-range rows and this (cursor-advanced) page
        // adds none: we've paged past the window. This is the order-agnostic
        // range-complete termination (holds for ascending or descending).
        assert!(!page_made_progress(Some("50"), Some("40"), 11, 11)); // descending
        assert!(!page_made_progress(Some("50"), Some("60"), 11, 11)); // ascending
    }

    #[test]
    fn keeps_paging_below_range_before_entering_window() {
        // Ascending relay whose early pages sit entirely below the window:
        // zero in-range rows yet, so we must keep paging to reach the window
        // rather than terminate after page 1 (the old descending-only bug).
        assert!(page_made_progress(Some("10"), Some("20"), 0, 0));
    }
}
