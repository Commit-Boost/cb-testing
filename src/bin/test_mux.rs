//! Quick mux routing diagnostic.
//!
//! Fetches CB PBS logs from a running enclave, parses them, and checks
//! mux routing against the provided config. No observation window, no
//! epoch waiting. Just: fetch → parse → check.
//!
//! Usage:
//!   cargo run --release --bin test_mux -- <enclave> <config>
//!
//! Example:
//!   cargo run --release --bin test_mux -- CB-Testnet configs/generated/cb-mux.yml

use std::process::Command;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: {} <enclave> <config>", args[0]);
        eprintln!("Example: {} CB-Testnet configs/generated/cb-mux.yml", args[0]);
        std::process::exit(1);
    }

    let enclave = &args[1];
    let config_path = &args[2];

    // Step 1: Parse mux config
    println!("=== Parsing mux config: {config_path} ===");
    let entries = match parse_mux_config(config_path) {
        Ok(Some(e)) => {
            println!("Found {} mux entries:", e.len());
            for entry in &e {
                println!("  {} → relay={} pubkeys={}", entry.id, entry.relay_identity, entry.validator_pubkeys.len());
            }
            e
        }
        Ok(None) => {
            println!("No [[mux]] sections found in config. Nothing to check.");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("ERROR parsing config: {e}");
            std::process::exit(1);
        }
    };

    // Step 2: Discover CB PBS services
    println!("\n=== Discovering CB PBS services in enclave: {enclave} ===");
    let services = match discover_services(enclave) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("ERROR discovering services: {e}");
            std::process::exit(1);
        }
    };
    println!("Found {} CB PBS service(s): {:?}", services.len(), services);

    if services.is_empty() {
        eprintln!("ERROR: No CB PBS services found");
        std::process::exit(1);
    }

    // Step 3: Fetch and parse logs from each service
    println!("\n=== Fetching logs ===");
    let mut all_events: Vec<CbEvent> = Vec::new();
    let log_file = format!("/tmp/test_mux_{}.log", enclave);

    for service in &services {
        println!("\n--- {service} ---");
        match fetch_logs(enclave, service) {
            Ok(logs) => {
                if logs.is_empty() {
                    println!("  (no relevant log lines found)");
                    continue;
                }
                // Write raw logs to file for debugging
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_file) {
                    use std::io::Write;
                    let _ = writeln!(f, "=== {service} ===");
                    let _ = writeln!(f, "{}", logs);
                }

                let mut parsed = 0;
                let mut failed = 0;
                for line in logs.lines() {
                    match parse_line(line) {
                        Some(event) => {
                            parsed += 1;
                            all_events.push(event);
                        }
                        None => {
                            failed += 1;
                            if failed <= 3 {
                                println!("  PARSE FAIL: {}", &line[..line.len().min(120)]);
                            }
                        }
                    }
                }
                println!("  Parsed: {parsed} lines, Failed: {failed} lines");
            }
            Err(e) => {
                println!("  ERROR: {e}");
            }
        }
    }
    println!("\nRaw logs written to: {log_file}");

    // Step 4: Print sample of ALL parsed events and collect unique messages
    let mut all_messages: Vec<String> = all_events.iter().map(|e| e.message.clone()).collect();
    all_messages.sort();
    all_messages.dedup();
    println!("\n=== Unique messages found ({} total) ===", all_messages.len());
    for msg in &all_messages {
        let count = all_events.iter().filter(|e| &e.message == msg).count();
        println!("  ({}) {}", count, msg);
    }

    println!("\n=== Sample of all parsed events (first 10) ===");
    for (i, event) in all_events.iter().take(10).enumerate() {
        let pk_short = event.validator.as_ref().map(|v| if v.len() > 16 { &v[..16] } else { v });
        println!("  #{} msg={:?} slot={:?} mux={:?} relay={:?} val={:?}",
            i, event.message, event.slot, event.mux_id, event.relay_id, pk_short);
    }
    if all_events.len() > 10 {
        println!("  ... and {} more", all_events.len() - 10);
    }

    // Step 5: Filter to mux events
    println!("\n=== Mux Events ===");
    let mux_events: Vec<&CbEvent> = all_events
        .iter()
        .filter(|e| {
            e.message.starts_with("using mux")
                || e.message.starts_with("received new header")
                || e.message.starts_with("auction winner")
        })
        .collect();

    println!("Total mux events: {}", mux_events.len());
    for event in &mux_events {
        let pk_short = event.validator.as_ref().map(|v| if v.len() > 20 { &v[..20] } else { v });
        println!(
            "  [{}] slot={:?} mux={:?} relay={:?} val={:?}",
            event.message,
            event.slot,
            event.mux_id,
            event.relay_id,
            pk_short
        );
    }

    // Step 5: Check mux routing
    println!("\n=== Mux Routing Check ===");
    let mut violations = 0;
    let mut checked = 0;

    for event in &mux_events {
        if let Some(ref pk) = event.validator {
            let pk_norm = pk.to_lowercase();
            for entry in &entries {
                if entry.validator_pubkeys.iter().any(|e| e.to_lowercase() == pk_norm) {
                    checked += 1;
                    if let Some(ref actual_mux) = event.mux_id
                        && actual_mux != &entry.id
                    {
                        violations += 1;
                        println!(
                            "  VIOLATION: pubkey {} should route to '{}' but routed to '{}'",
                            &pk[..20.min(pk.len())],
                            entry.id,
                            actual_mux
                        );
                    }
                }
            }
        }
    }

    println!("\n=== Result ===");
    if violations > 0 {
        println!("FAIL: {violations} routing violation(s) out of {checked} checked");
        std::process::exit(1);
    } else if checked == 0 {
        println!("WARN: No mux events matched to config pubkeys. Events may not have proposer_pubkey fields.");
        std::process::exit(0);
    } else {
        println!("PASS: All {checked} mux routing decisions are correct");
        std::process::exit(0);
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

struct MuxEntry {
    id: String,
    relay_identity: String,
    validator_pubkeys: Vec<String>,
}

struct CbEvent {
    message: String,
    slot: Option<u64>,
    validator: Option<String>,
    relay_id: Option<String>,
    mux_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Config parsing
// ---------------------------------------------------------------------------

fn parse_mux_config(path: &str) -> Result<Option<Vec<MuxEntry>>, Box<dyn std::error::Error>> {
    let raw = std::fs::read_to_string(path)?;

    let template = if path.ends_with(".yml") || path.ends_with(".yaml") {
        let parsed: serde_yaml::Value = serde_yaml::from_str(&raw)?;
        parsed
            .get("mev_params")
            .and_then(|p| p.get("commit_boost_config"))
            .and_then(|c| c.as_str())
            .ok_or("No mev_params.commit_boost_config found")?
            .to_string()
    } else {
        raw
    };

    if !template.contains("[[mux]]") {
        return Ok(None);
    }

    let mut entries = Vec::new();
    let mut lines = template.lines().peekable();

    while let Some(line) = lines.next() {
        if line.trim() == "[[mux]]" {
            let entry = parse_mux_section(&mut lines);
            entries.push(entry);
        }
    }

    if entries.is_empty() {
        return Ok(None);
    }

    Ok(Some(entries))
}

fn parse_mux_section<'a>(lines: &mut std::iter::Peekable<std::str::Lines<'a>>) -> MuxEntry {
    let mut id = None;
    let mut pubkeys = None;

    loop {
        let is_header = lines.peek().map(|l| l.trim().starts_with("[[")).unwrap_or(false);
        if is_header {
            let header = lines.peek().unwrap().trim().to_string();
            if header.starts_with("[[mux.relays]]") {
                let _ = lines.next();
                continue;
            }
            break;
        }

        let Some(line) = lines.next() else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some((key, val)) = trimmed.split_once('=') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "id" => id = Some(val.trim_matches('"').to_string()),
                "validator_pubkeys" => pubkeys = Some(parse_pubkey_array(val, lines)),
                _ => {}
            }
        }
    }

    let id = id.unwrap_or_default();
    let relay_identity = if let Some(pos) = id.rfind("to_") {
        let ident = id[pos + 3..].trim().to_string();
        if !ident.is_empty() { ident } else { id.clone() }
    } else {
        id.clone()
    };
    let pubkeys = pubkeys.unwrap_or_default();

    MuxEntry { id, relay_identity, validator_pubkeys: pubkeys }
}

fn parse_pubkey_array(rest: &str, lines: &mut std::iter::Peekable<std::str::Lines>) -> Vec<String> {
    let mut accum = rest.to_string();
    if !accum.trim_end().ends_with(']') {
        for next in lines.by_ref() {
            accum.push('\n');
            accum.push_str(next);
            if next.trim().ends_with(']') { break; }
        }
    }
    let raw = accum.trim();
    let start = raw.find('[').unwrap_or(0);
    let end = raw.rfind(']').unwrap_or(raw.len());
    raw[start + 1..end]
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Service discovery
// ---------------------------------------------------------------------------

fn discover_services(enclave: &str) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let output = Command::new("kurtosis")
        .args(["enclave", "inspect", "--full-uuids", enclave])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("kurtosis enclave inspect failed: {stderr}").into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut services = Vec::new();

    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("commit-boost") && lower.contains("running") {
            // Extract service name (first column)
            if let Some(name) = line.split_whitespace().next() {
                services.push(name.to_string());
            }
        }
    }

    Ok(services)
}

// ---------------------------------------------------------------------------
// Log fetching
// ---------------------------------------------------------------------------

fn fetch_logs(enclave: &str, service: &str) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("kurtosis")
        .args(["service", "logs", enclave, service, "-n", "200000"])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("kurtosis service logs failed: {stderr}").into());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Filter to relevant lines
    let result: String = stdout
        .lines()
        .filter(|line| {
            line.contains("using mux config")
                || line.contains("received new header")
                || line.contains("auction winner")
                || line.contains("received unblinded block")
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok(result)
}

// ---------------------------------------------------------------------------
// Log parsing
// ---------------------------------------------------------------------------

fn strip_ansi_codes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&ch) = chars.peek() {
                    chars.next();
                    if ch.is_ascii_alphabetic() { break; }
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn parse_line(line: &str) -> Option<CbEvent> {
    let line = line.trim();
    if line.is_empty() { return None; }

    // Strip kurtosis prefix: "[service-name] rest"
    let line = if line.starts_with('[') {
        if let Some(pos) = line.find(']') {
            line[pos + 1..].trim_start()
        } else { line }
    } else { line };

    // Strip ANSI escape codes
    let line = strip_ansi_codes(line);

    // Find message after "LEVEL : " or "LEVEL "
    let after_level: String = if let Some(pos) = line.find(" : ") {
        line[pos + 3..].to_string()
    } else {
        let levels = ["TRACE", "DEBUG", "INFO", "WARN", "ERROR"];
        let mut found = line.clone();
        for lvl in &levels {
            if let Some(pos) = line.find(&format!(" {} ", lvl)) {
                found = line[pos + lvl.len() + 2..].to_string();
                break;
            }
            if let Some(stripped) = line.strip_prefix(lvl) {
                found = stripped.trim_start().to_string();
                break;
            }
        }
        found
    };

    // Find message/key boundary: first " key=" where key is a valid identifier
    let mut message_end = after_level.len();
    let bytes = after_level.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b' ' {
            let rest = &after_level[i + 1..];
            if rest.is_empty() { continue; }
            let first = rest.as_bytes()[0];
            if (first.is_ascii_alphabetic() || first == b'_')
                && let Some(eq_pos) = rest.find('=')
            {
                let key = &rest[..eq_pos];
                if key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    let after_eq = &rest[eq_pos + 1..];
                    if !after_eq.is_empty() {
                        message_end = i;
                        break;
                    }
                }
            }
        }
    }

    let message = after_level[..message_end].trim().to_string();
    let kv_part = &after_level[message_end..];

    let mut slot = None;
    let mut validator = None;
    let mut relay_id = None;
    let mut mux_id = None;

    for kv in kv_part.split_whitespace() {
        if let Some((key, val)) = kv.split_once('=') {
            let val = val.trim_matches('"');
            match key {
                "slot" => { slot = val.parse().ok(); }
                "validator" | "pubkey" => { validator = Some(val.to_string()); }
                "relay_id" => { relay_id = Some(val.to_string()); }
                "mux_id" => { mux_id = Some(val.to_string()); }
                _ => {}
            }
        }
    }

    Some(CbEvent { message, slot, validator, relay_id, mux_id })
}
