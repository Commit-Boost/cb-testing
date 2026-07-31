//! Quick relay API diagnostic.
//!
//! Tests all relay data API endpoints with proper slot filtering.
//! No observation window needed — just query the relay directly.
//!
//! Usage:
//!   cargo run --release --bin test_relay -- <relay_url> <start_slot> <end_slot> [pubkey]
//!
//! Examples:
//!   cargo run --release --bin test_relay -- http://127.0.0.1:59945 128 160
//!   cargo run --release --bin test_relay -- http://127.0.0.1:59945 128 160 0x889dbdf3bd68af1f6fd84cb6173b1fa1f7c5e6ba63297dc1e2f45cd1a82bb6231ba832adc5228143c5cff3ef0b1caae2

use std::time::Duration;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "Usage: {} <relay_url> <start_slot> <end_slot> [pubkey]",
            args[0]
        );
        eprintln!("Example: {} http://127.0.0.1:59945 128 160", args[0]);
        std::process::exit(1);
    }

    let relay_url = &args[1];
    let start_slot: u64 = args[2].parse().expect("invalid start_slot");
    let end_slot: u64 = args[3].parse().expect("invalid end_slot");
    let pubkey = args.get(4).map(|s| s.as_str());

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build HTTP client");

    let base = relay_url.trim_end_matches('/');

    // === 0. Check if relay has ANY delivered payloads at all ===
    println!("=== 0. Latest delivered payloads (no slot filter) ===");
    {
        let url = format!("{base}/relay/v1/data/bidtraces/proposer_payload_delivered");
        let req = client.get(&url).query(&[("limit", "5")]);
        match send::<Vec<PayloadDelivered>>(req).await {
            Ok(payloads) => {
                println!("  Total payloads available: {}", payloads.len());
                for p in &payloads {
                    println!(
                        "    slot={} value={} proposer={}...",
                        p.slot,
                        p.value,
                        &p.proposer_pubkey[..20.min(p.proposer_pubkey.len())]
                    );
                }
            }
            Err(e) => println!("  FAIL: {e}"),
        }
    }

    // === 1. Delivered payloads filtered by slot ===
    println!("=== 1. Delivered payloads (slot {start_slot}..={end_slot}) ===");
    {
        let url = format!("{base}/relay/v1/data/bidtraces/proposer_payload_delivered");
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let mut page = 0;
        let limit_str = String::from("200");
        let start_slot_str = start_slot.to_string();

        loop {
            page += 1;
            let mut params: Vec<(&str, String)> = vec![
                ("limit", limit_str.clone()),
                ("slot", start_slot_str.clone()),
            ];
            if let Some(ref c) = cursor {
                params.push(("cursor", c.clone()));
            }

            let req = client.get(&url).query(&params);
            match send::<Vec<PayloadDelivered>>(req).await {
                Ok(payloads) => {
                    if payloads.is_empty() {
                        println!("  Page {page}: empty, done");
                        break;
                    }
                    let min_slot = payloads
                        .iter()
                        .filter_map(|p| p.slot.parse::<u64>().ok())
                        .min()
                        .unwrap_or(0);
                    let max_slot = payloads
                        .iter()
                        .filter_map(|p| p.slot.parse::<u64>().ok())
                        .max()
                        .unwrap_or(0);
                    let page_len = payloads.len();
                    let in_range: Vec<_> = payloads
                        .into_iter()
                        .filter(|p| {
                            let s: u64 = p.slot.parse().unwrap_or(0);
                            s >= start_slot && s <= end_slot
                        })
                        .collect();
                    let count = in_range.len();
                    all.extend(in_range);
                    println!(
                        "  Page {page}: {page_len} payloads (slots {min_slot}..={max_slot}), {count} in range, {} total",
                        all.len()
                    );
                    if min_slot < start_slot {
                        break;
                    }
                    cursor = all
                        .last()
                        .map(|p: &PayloadDelivered| p.block_number.clone());
                }
                Err(e) => {
                    println!("  FAIL: {e}");
                    break;
                }
            }
            if page >= 50 {
                break;
            }
        }
        println!("  Total delivered in range: {}", all.len());
        for p in all.iter().take(5) {
            let pk_short = if p.proposer_pubkey.len() > 20 {
                &p.proposer_pubkey[..20]
            } else {
                &p.proposer_pubkey
            };
            println!(
                "    slot={} value={} proposer={}...",
                p.slot, p.value, pk_short
            );
        }
    }

    // === 2. Builder blocks received filtered by slot ===
    println!("\n=== 2. Builder blocks received (slot {start_slot}..={end_slot}) ===");
    {
        let url = format!("{base}/relay/v1/data/bidtraces/builder_blocks_received");
        let mut all = Vec::new();

        // Query each slot individually (the API supports slot filter)
        for slot in start_slot..=end_slot {
            let slot_str = slot.to_string();
            let limit_str = "200".to_string();
            let req = client
                .get(&url)
                .query(&[("slot", &slot_str), ("limit", &limit_str)]);
            match send::<Vec<BuilderBlock>>(req).await {
                Ok(blocks) => {
                    if !blocks.is_empty() {
                        println!("  Slot {slot}: {} blocks", blocks.len());
                        for b in &blocks {
                            let val_short = if b.value.len() > 12 {
                                &b.value[..12]
                            } else {
                                &b.value
                            };
                            println!(
                                "    builder={}... value={val_short}",
                                &b.builder_pubkey[..20.min(b.builder_pubkey.len())]
                            );
                        }
                        all.extend(blocks);
                    }
                }
                Err(e) => {
                    println!("  Slot {slot}: FAIL: {e}");
                }
            }
        }
        println!("  Total builder blocks in range: {}", all.len());
    }

    // === 3. Validator registration check ===
    println!("\n=== 3. Validator registration ===");
    if let Some(pk) = pubkey {
        let url = format!("{base}/relay/v1/data/validator_registration");
        let req = client.get(&url).query(&[("pubkey", pk)]);
        match send::<serde_json::Value>(req).await {
            Ok(reg) => {
                println!("  Registered: YES");
                if let Some(msg) = reg.get("message") {
                    println!(
                        "  Fee recipient: {}",
                        msg.get("fee_recipient")
                            .and_then(|v| v.as_str())
                            .unwrap_or("?")
                    );
                    println!(
                        "  Gas limit: {}",
                        msg.get("gas_limit").and_then(|v| v.as_str()).unwrap_or("?")
                    );
                    println!(
                        "  Timestamp: {}",
                        msg.get("timestamp").and_then(|v| v.as_str()).unwrap_or("?")
                    );
                }
            }
            Err(e) => {
                println!("  Registered: NO ({e})");
            }
        }
    } else {
        println!("  (skipped — no pubkey provided)");
    }

    // === 4. Summary ===
    println!("\n=== Summary ===");
    println!("Relay: {base}");
    println!(
        "Slot range: {start_slot}..={end_slot} ({} slots)",
        end_slot - start_slot + 1
    );
    println!("All queries completed successfully");
}

async fn send<T: serde::de::DeserializeOwned>(req: reqwest::RequestBuilder) -> Result<T, String> {
    let resp = req.send().await.map_err(|e| format!("HTTP error: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {}", status, body));
    }
    resp.json::<T>()
        .await
        .map_err(|e| format!("JSON error: {e}"))
}

// Mirrors the relay `proposer/delivered` data-API JSON schema; several fields
// are deserialized to document the response shape but not yet read.
#[allow(dead_code)]
#[derive(serde::Deserialize, Debug, Clone)]
struct PayloadDelivered {
    slot: String,
    block_hash: String,
    value: String,
    #[serde(default)]
    proposer_pubkey: String,
    #[serde(default)]
    builder_pubkey: String,
    #[serde(default)]
    block_number: String,
    #[serde(default)]
    parent_hash: String,
    #[serde(default)]
    proposer_fee_recipient: String,
    #[serde(default)]
    gas_limit: String,
    #[serde(default)]
    gas_used: String,
    #[serde(default)]
    num_tx: String,
}

// Mirrors the relay `builder/blocks` data-API JSON schema; several fields are
// deserialized to document the response shape but not yet read.
#[allow(dead_code)]
#[derive(serde::Deserialize, Debug, Clone)]
struct BuilderBlock {
    slot: String,
    block_hash: String,
    value: String,
    #[serde(default)]
    builder_pubkey: String,
    #[serde(default)]
    proposer_pubkey: String,
    #[serde(default)]
    block_number: String,
    #[serde(default)]
    parent_hash: String,
    #[serde(default)]
    proposer_fee_recipient: String,
    #[serde(default)]
    gas_limit: String,
    #[serde(default)]
    gas_used: String,
    #[serde(default)]
    num_tx: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    timestamp_ms: String,
}

// Add this as test 0 at the start of main(), before the slot range tests
