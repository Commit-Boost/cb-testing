//! Prometheus metrics fetching and parsing.
//!
//! Uses the `prometheus-parse` crate for parsing the text exposition format.
//! Supports both direct HTTP fetch and kurtosis exec fallback.

use std::process::Command;

use eyre::{Result, WrapErr, bail};
use prometheus_parse::{Scrape, Value};
use tracing::{debug, warn};

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

/// Helper: sum all samples matching a metric name and optional label filter.
pub fn sum_metric(scrape: &Scrape, name: &str, label_filter: Option<(&str, &str)>) -> f64 {
    scrape
        .samples
        .iter()
        .filter(|s| s.metric == name)
        .filter(|s| {
            if let Some((key, val)) = label_filter {
                s.labels.get(key).map(|v| v.as_ref()) == Some(val)
            } else {
                true
            }
        })
        .map(|s| match &s.value {
            Value::Counter(v) | Value::Gauge(v) | Value::Untyped(v) => *v,
            _ => 0.0,
        })
        .sum()
}

/// Helper: check if any samples exist for a metric.
pub fn has_metric(scrape: &Scrape, name: &str) -> bool {
    scrape.samples.iter().any(|s| s.metric == name)
}

/// Helper: get all sample values for a metric, optionally filtered by label.
pub fn metric_values(scrape: &Scrape, name: &str, label_filter: Option<(&str, &str)>) -> Vec<f64> {
    scrape
        .samples
        .iter()
        .filter(|s| s.metric == name)
        .filter(|s| {
            if let Some((key, val)) = label_filter {
                s.labels.get(key).map(|v| v.as_ref()) == Some(val)
            } else {
                true
            }
        })
        .filter_map(|s| match &s.value {
            Value::Counter(v) | Value::Gauge(v) | Value::Untyped(v) => Some(*v),
            _ => None,
        })
        .collect()
}
