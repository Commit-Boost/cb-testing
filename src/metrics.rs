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

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape CB exposes, captured from a live devnet scrape. Every
    /// matrix check reads these three families, so a parse regression here
    /// makes them all SKIP - silently, since "metrics absent" is the normal
    /// devnet state and SKIP is non-fatal.
    const CB_SCRAPE: &str = r#"# HELP cb_pbs_relay_status_code_total relay status codes
# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{endpoint="get_header",http_status_code="200",relay_id="mev_relay_0"} 27
cb_pbs_relay_status_code_total{endpoint="get_header",http_status_code="555",relay_id="mev_relay_0"} 17
# HELP pbs_submit_block_v2_unsupported_total v2 unsupported
# TYPE pbs_submit_block_v2_unsupported_total counter
pbs_submit_block_v2_unsupported_total{relay_id="mev_relay_0"} 11
# HELP cb_pbs_relay_latency HTTP latency by relay
# TYPE cb_pbs_relay_latency histogram
cb_pbs_relay_latency_bucket{endpoint="get_header",relay_id="mev_relay_0",le="0.05"} 12
cb_pbs_relay_latency_bucket{endpoint="get_header",relay_id="mev_relay_0",le="+Inf"} 27
cb_pbs_relay_latency_sum{endpoint="get_header",relay_id="mev_relay_0"} 1.5
cb_pbs_relay_latency_count{endpoint="get_header",relay_id="mev_relay_0"} 27
"#;

    #[test]
    fn parses_a_real_cb_scrape_with_labels_and_histograms() {
        let scrape = parse_metrics(CB_SCRAPE).expect("real CB scrape must parse");
        let names: Vec<&str> = scrape.samples.iter().map(|s| s.metric.as_str()).collect();
        assert!(names.contains(&"cb_pbs_relay_status_code_total"));
        assert!(names.contains(&"pbs_submit_block_v2_unsupported_total"));
        // Labels must survive: every check keys on endpoint/http_status_code/relay_id.
        let s = scrape
            .samples
            .iter()
            .find(|s| {
                s.metric == "cb_pbs_relay_status_code_total"
                    && s.labels.get("http_status_code") == Some("555")
            })
            .expect("the synthetic 555 sample must be addressable by label");
        assert_eq!(s.labels.get("relay_id"), Some("mev_relay_0"));
    }

    #[test]
    fn empty_scrape_parses_to_no_samples_rather_than_erroring() {
        // The default devnet exposes no metrics; that must be an empty scrape
        // (checks then SKIP), never a hard error that fails the run.
        let scrape = parse_metrics("").expect("empty body must parse");
        assert!(scrape.samples.is_empty());
    }

    #[test]
    fn comments_only_scrape_is_empty() {
        let scrape = parse_metrics("# HELP x nothing\n# TYPE x counter\n").unwrap();
        assert!(scrape.samples.is_empty());
    }

    #[tokio::test]
    async fn fetch_from_a_dead_endpoint_errors_instead_of_hanging() {
        // Port 1 refuses instantly. The caller turns this into a SKIP; it must
        // never panic or block the run.
        let client = reqwest::Client::new();
        assert!(fetch_metrics(&client, "http://127.0.0.1:1").await.is_err());
    }
}
