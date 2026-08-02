//! Prometheus metrics fetching and parsing.
//!
//! Uses the `prometheus-parse` crate for parsing the text exposition format.
//! Supports both direct HTTP fetch and kurtosis exec fallback.

use std::process::Command;

use eyre::{Result, WrapErr, bail};
use prometheus_parse::Scrape;

/// Fetch and parse Prometheus metrics from an HTTP endpoint.
pub async fn fetch_metrics(client: &reqwest::Client, url: &str) -> Result<Scrape> {
    let text = client
        .get(format!("{}/metrics", url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    parse_metrics(&text)
}

/// Fetch metrics via `kurtosis service exec` when the port isn't exposed to the host.
pub fn fetch_metrics_via_exec(enclave: &str, service: &str, port: u16) -> Result<Scrape> {
    // TODO: Replace with Kurtosis Rust SDK when available.
    let output = Command::new("kurtosis")
        .args([
            "service",
            "exec",
            enclave,
            service,
            &format!("curl -s http://localhost:{port}/metrics"),
        ])
        .output()
        .wrap_err("kurtosis CLI not found")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("kurtosis exec failed: {}", stderr.trim());
    }

    let text = String::from_utf8_lossy(&output.stdout);
    parse_metrics(&text)
}

/// Parse Prometheus text exposition format.
fn parse_metrics(text: &str) -> Result<Scrape> {
    let lines = text.lines().map(|l| Ok(l.to_owned()));
    Scrape::parse(lines).wrap_err("failed to parse Prometheus metrics")
}
