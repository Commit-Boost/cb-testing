//! Prometheus metrics fetching and parsing.
//!
//! Uses the `prometheus-parse` crate for parsing the text exposition format.
//! Supports both direct HTTP fetch and kurtosis exec fallback.

use std::collections::HashMap;
use std::process::Command;

use eyre::{Result, WrapErr, bail};
use prometheus_parse::{HistogramCount, Sample, Scrape, Value};

/// Fetch and parse Prometheus metrics from an HTTP endpoint.
pub async fn fetch_metrics(client: &reqwest::Client, url: &str) -> Result<Scrape> {
    parse_metrics(&fetch_metrics_text(client, url).await?)
}

/// The raw exposition text of an HTTP `/metrics` endpoint.
pub async fn fetch_metrics_text(client: &reqwest::Client, url: &str) -> Result<String> {
    Ok(client
        .get(format!("{}/metrics", url.trim_end_matches('/')))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// Fetch metrics via `kurtosis service exec` when the port isn't exposed to the host.
pub fn fetch_metrics_via_exec(enclave: &str, service: &str, port: u16) -> Result<Scrape> {
    parse_metrics(&fetch_metrics_text_via_exec(enclave, service, port)?)
}

/// [`fetch_metrics_via_exec`], unparsed.
pub fn fetch_metrics_text_via_exec(enclave: &str, service: &str, port: u16) -> Result<String> {
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

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse Prometheus text exposition format.
pub fn parse_metrics(text: &str) -> Result<Scrape> {
    let lines = text.lines().map(|l| Ok(l.to_owned()));
    Scrape::parse(lines).wrap_err("failed to parse Prometheus metrics")
}

/// What CB counted between two scrapes of the same process.
#[derive(Debug, Clone)]
pub struct Window {
    /// Reads as the scrape of a CB that started at the baseline: counters and
    /// histograms hold the increase, gauges their final value, and a series
    /// that did not move is absent, as prometheus omits a never-touched series.
    pub scrape: Scrape,
    /// Series that went backwards. Non-empty means CB restarted in between,
    /// and `scrape` is the final scrape as-is: the counts since the restart.
    pub resets: Vec<String>,
}

/// `end` minus `baseline`, per metric and label set.
pub fn window(baseline: &Scrape, end: &Scrape) -> Window {
    let before: HashMap<(&str, String), &Value> = baseline
        .samples
        .iter()
        .map(|s| ((s.metric.as_str(), s.labels.to_string()), &s.value))
        .collect();
    let base_of = |s: &Sample| {
        before
            .get(&(s.metric.as_str(), s.labels.to_string()))
            .copied()
    };

    // Registered counters are process-wide, so one series going backwards means
    // every series restarted from zero, including those that have since passed
    // their baseline; subtracting from those would undercount
    let resets: Vec<String> = end
        .samples
        .iter()
        .filter(|s| base_of(s).is_some_and(|b| went_backwards(s, b)))
        .map(|s| format!("{}{{{}}}", s.metric, s.labels))
        .collect();
    if !resets.is_empty() {
        return Window {
            scrape: end.clone(),
            resets,
        };
    }

    let samples = end
        .samples
        .iter()
        .filter_map(|s| {
            let value = match (&s.value, base_of(s)) {
                (_, None) => s.value.clone(),
                (Value::Counter(v), Some(Value::Counter(b))) => Value::Counter(v - b),
                (Value::Untyped(v), Some(Value::Untyped(b))) if accumulates(&s.metric) => {
                    Value::Untyped(v - b)
                }
                (Value::Histogram(buckets), Some(Value::Histogram(base))) => Value::Histogram(
                    buckets
                        .iter()
                        .map(|hc| HistogramCount {
                            less_than: hc.less_than,
                            count: hc.count - bucket_at(base, hc.less_than),
                        })
                        .collect(),
                ),
                _ => return Some(s.clone()),
            };
            let moved = match &value {
                Value::Counter(v) | Value::Untyped(v) => *v != 0.0,
                Value::Histogram(buckets) => buckets.iter().any(|hc| hc.count != 0.0),
                _ => true,
            };
            moved.then(|| Sample { value, ..s.clone() })
        })
        .collect();

    Window {
        scrape: Scrape {
            docs: end.docs.clone(),
            samples,
        },
        resets: Vec::new(),
    }
}

/// A histogram's `_sum`/`_count` and an untyped `_total` parse as `Untyped`;
/// by naming convention they accumulate like counters.
fn accumulates(metric: &str) -> bool {
    ["_total", "_count", "_sum"]
        .iter()
        .any(|suffix| metric.ends_with(suffix))
}

fn bucket_at(buckets: &[HistogramCount], le: f64) -> f64 {
    buckets
        .iter()
        .find(|hc| hc.less_than == le)
        .map_or(0.0, |hc| hc.count)
}

/// A `_sum` can shrink on negative observations, so it is no reset signal.
fn went_backwards(end: &Sample, base: &Value) -> bool {
    match (&end.value, base) {
        (Value::Counter(v), Value::Counter(b)) => v < b,
        (Value::Untyped(v), Value::Untyped(b)) => {
            accumulates(&end.metric) && !end.metric.ends_with("_sum") && v < b
        }
        (Value::Histogram(buckets), Value::Histogram(base)) => buckets
            .iter()
            .any(|hc| hc.count < bucket_at(base, hc.less_than)),
        _ => false,
    }
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

    fn value(scrape: &Scrape, metric: &str, label: (&str, &str)) -> Option<Value> {
        scrape
            .samples
            .iter()
            .find(|s| s.metric == metric && s.labels.get(label.0) == Some(label.1))
            .map(|s| s.value.clone())
    }

    const BASELINE: &str = r#"# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{endpoint="get_header",http_status_code="200",relay_id="mev_relay_0"} 10
cb_pbs_relay_status_code_total{endpoint="get_header",http_status_code="400",relay_id="mev_relay_0"} 33
# TYPE cb_pbs_relay_latency histogram
cb_pbs_relay_latency_bucket{endpoint="get_header",relay_id="mev_relay_0",le="0.05"} 12
cb_pbs_relay_latency_bucket{endpoint="get_header",relay_id="mev_relay_0",le="+Inf"} 27
cb_pbs_relay_latency_sum{endpoint="get_header",relay_id="mev_relay_0"} 1.5
cb_pbs_relay_latency_count{endpoint="get_header",relay_id="mev_relay_0"} 27
# TYPE cb_pbs_active_relays gauge
cb_pbs_active_relays 2
"#;

    const END: &str = r#"# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{endpoint="get_header",http_status_code="200",relay_id="mev_relay_0"} 41
cb_pbs_relay_status_code_total{endpoint="get_header",http_status_code="400",relay_id="mev_relay_0"} 33
cb_pbs_relay_status_code_total{endpoint="get_header",http_status_code="204",relay_id="mev_relay_0"} 4
# TYPE cb_pbs_relay_latency histogram
cb_pbs_relay_latency_bucket{endpoint="get_header",relay_id="mev_relay_0",le="0.05"} 30
cb_pbs_relay_latency_bucket{endpoint="get_header",relay_id="mev_relay_0",le="+Inf"} 62
cb_pbs_relay_latency_sum{endpoint="get_header",relay_id="mev_relay_0"} 4
cb_pbs_relay_latency_count{endpoint="get_header",relay_id="mev_relay_0"} 62
# TYPE cb_pbs_active_relays gauge
cb_pbs_active_relays 1
"#;

    fn windowed() -> Window {
        window(
            &parse_metrics(BASELINE).unwrap(),
            &parse_metrics(END).unwrap(),
        )
    }

    #[test]
    fn window_subtracts_counters_per_label_set() {
        let w = windowed();
        assert!(w.resets.is_empty());
        let status = "cb_pbs_relay_status_code_total";
        let code = "http_status_code";
        assert_eq!(
            value(&w.scrape, status, (code, "200")),
            Some(Value::Counter(31.0))
        );
        // new since the baseline: counted from zero
        assert_eq!(
            value(&w.scrape, status, (code, "204")),
            Some(Value::Counter(4.0))
        );
        // did not move: absent, as on a CB that started at the baseline
        assert_eq!(value(&w.scrape, status, (code, "400")), None);
        // absent in both stays absent
        assert_eq!(value(&w.scrape, status, (code, "500")), None);
    }

    #[test]
    fn window_subtracts_histogram_buckets_count_and_sum() {
        let w = windowed();
        let label = ("relay_id", "mev_relay_0");
        let Some(Value::Histogram(buckets)) = value(&w.scrape, "cb_pbs_relay_latency", label)
        else {
            panic!("the latency histogram must survive windowing");
        };
        let buckets: Vec<(f64, f64)> = buckets.iter().map(|b| (b.less_than, b.count)).collect();
        assert_eq!(buckets, vec![(0.05, 18.0), (f64::INFINITY, 35.0)]);
        assert_eq!(
            value(&w.scrape, "cb_pbs_relay_latency_count", label),
            Some(Value::Untyped(35.0))
        );
        assert_eq!(
            value(&w.scrape, "cb_pbs_relay_latency_sum", label),
            Some(Value::Untyped(2.5))
        );
    }

    #[test]
    fn window_keeps_a_gauge_at_its_final_value() {
        let w = windowed();
        let gauge = w
            .scrape
            .samples
            .iter()
            .find(|s| s.metric == "cb_pbs_active_relays")
            .map(|s| s.value.clone());
        assert_eq!(gauge, Some(Value::Gauge(1.0)));
    }

    #[test]
    fn a_counter_that_went_backwards_is_a_restart_and_judges_the_final_counts() {
        // CB restarted mid-window: 400 fell from 33 to 5, while 200 climbed
        // past its baseline on post-restart traffic alone
        let end = END
            .replace(
                r#"http_status_code="400",relay_id="mev_relay_0"} 33"#,
                r#"http_status_code="400",relay_id="mev_relay_0"} 5"#,
            )
            .replace(
                r#"http_status_code="200",relay_id="mev_relay_0"} 41"#,
                r#"http_status_code="200",relay_id="mev_relay_0"} 12"#,
            );
        let w = window(
            &parse_metrics(BASELINE).unwrap(),
            &parse_metrics(&end).unwrap(),
        );
        assert_eq!(w.resets.len(), 1, "{:?}", w.resets);
        assert!(
            w.resets[0].contains(r#"http_status_code="400""#),
            "{:?}",
            w.resets
        );
        let status = "cb_pbs_relay_status_code_total";
        let code = "http_status_code";
        // never negative, and never baseline-subtracted after a restart
        assert_eq!(
            value(&w.scrape, status, (code, "400")),
            Some(Value::Counter(5.0))
        );
        assert_eq!(
            value(&w.scrape, status, (code, "200")),
            Some(Value::Counter(12.0))
        );
    }

    #[test]
    fn a_shrinking_histogram_bucket_is_a_restart() {
        let end = END.replace(r#"le="+Inf"} 62"#, r#"le="+Inf"} 3"#);
        let w = window(
            &parse_metrics(BASELINE).unwrap(),
            &parse_metrics(&end).unwrap(),
        );
        assert_eq!(w.resets.len(), 1, "{:?}", w.resets);
        assert!(
            w.resets[0].starts_with("cb_pbs_relay_latency{"),
            "{:?}",
            w.resets
        );
    }

    #[tokio::test]
    async fn fetch_from_a_dead_endpoint_errors_instead_of_hanging() {
        // Port 1 refuses instantly. The caller turns this into a SKIP; it must
        // never panic or block the run.
        let client = reqwest::Client::new();
        assert!(fetch_metrics(&client, "http://127.0.0.1:1").await.is_err());
    }
}
