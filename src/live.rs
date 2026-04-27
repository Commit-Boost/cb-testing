//! Live metrics polling for cb-verify.
//!
//! Adds real-time delta logging during the observation window. Scrapes metrics
//! every 30s, computes counter deltas vs prior scrape, and logs a one-line summary
//! per endpoint with non-zero changes. Turns a silent 12-minute wait into a live feed.
//!
/// Only samples:
/// - cb_pbs_relay_status_code_total
/// - cb_pbs_beacon_node_status_code_total
/// - cb_pbs_submit_block_v2_fallback_to_v1_total
///
/// Histograms (cb_pbs_relay_latency) are skipped: cumulative, low signal per 30s.
///
/// JSON mode: deltas go to stderr as NDJSON when --json is set.
use std::collections::HashMap;

/// The metric families we track for live delta computation.
/// Histogram families excluded (low signal per 30s scrape).
pub const LIVE_METRICS_FILTER: &[&str] = &[
    "cb_pbs_relay_status_code_total",
    "cb_pbs_beacon_node_status_code_total",
    "cb_pbs_submit_block_v2_fallback_to_v1_total",
];

use prometheus_parse::Scrape;

/// A single delta row between two metric scrapes.
#[derive(Debug, Clone)]
pub struct DeltaRow {
    pub endpoint: String,
    pub relay: Option<String>,
    pub deltas: HashMap<String, i64>, // status_code -> delta count
}

impl DeltaRow {
    pub fn new(endpoint: String, relay: Option<String>, deltas: HashMap<String, i64>) -> Self {
        Self {
            endpoint,
            relay,
            deltas,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut json = serde_json::Map::new();
        json.insert(
            "ts".to_string(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
        json.insert(
            "endpoint".to_string(),
            serde_json::Value::String(self.endpoint.clone()),
        );
        if let Some(relay) = &self.relay {
            json.insert(
                "relay".to_string(),
                serde_json::Value::String(relay.clone()),
            );
        }
        let deltas_json: serde_json::Map<String, serde_json::Value> = self
            .deltas
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    serde_json::Value::Number(serde_json::Number::from(*v)),
                )
            })
            .collect();
        json.insert("deltas".to_string(), serde_json::Value::Object(deltas_json));
        serde_json::Value::Object(json)
    }

    pub fn to_log_line(&self) -> String {
        let label = format!("live: {}", self.endpoint);
        let mut deltas: Vec<String> = Vec::new();
        let mut codes: Vec<&String> = self.deltas.keys().collect();
        codes.sort();
        for code in codes {
            if let Some(delta) = self.deltas.get(code) {
                if *delta > 0 {
                    deltas.push(format!("+{delta} {code}"));
                }
            }
        }
        let mut line = label;
        if !deltas.is_empty() {
            line.push(' ');
            line.push_str(&deltas.join(", "));
        }
        if let Some(relay) = &self.relay {
            line.push_str(&format!(" ({relay})"));
        }
        line
    }
}

/// Compute deltas between two metric scrapes.
///
/// Returns a Vec of DeltaRow for every endpoint + relay + status_code combo
/// that changed.
///
/// Ignores histograms. Only considers counter families:
///   - cb_pbs_relay_status_code_total
///   - cb_pbs_beacon_node_status_code_total
///   - cb_pbs_submit_block_v2_fallback_to_v1_total
///
/// If current scrape is missing a label, treat as zero.
/// If current scrape has a lower counter than prev, treat as zero (counter reset).
///
/// Returns empty Vec if either scrape is empty or if no relevant metrics found.
pub fn compute_deltas(
    prev: Option<&Scrape>,
    curr: &Scrape,
    metric_filter: &[&str],
) -> Vec<DeltaRow> {
    let mut deltas = Vec::new();

    for metric in metric_filter {
        match *metric {
            "cb_pbs_relay_status_code_total" => {
                for sample in &curr.samples {
                    if sample.metric != "cb_pbs_relay_status_code_total" {
                        continue;
                    }
                    let endpoint = match sample.labels.get("endpoint") {
                        Some(e) => e.to_string(),
                        None => continue,
                    };
                    let code = match sample.labels.get("http_status_code") {
                        Some(c) => c.to_string(),
                        None => continue,
                    };
                    let relay_id = sample
                        .labels
                        .get("relay_id")
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    let curr_val = match &sample.value {
                        prometheus_parse::Value::Counter(v)
                        | prometheus_parse::Value::Gauge(v)
                        | prometheus_parse::Value::Untyped(v) => *v as i64,
                        _ => continue,
                    };

                    let prev_val = if let Some(prev) = prev {
                        prev.samples
                            .iter()
                            .find(|s| {
                                s.metric == "cb_pbs_relay_status_code_total"
                                    && s.labels.get("endpoint") == Some(&endpoint)
                                    && s.labels.get("http_status_code") == Some(&code)
                                    && s.labels.get("relay_id") == Some(&relay_id)
                            })
                            .and_then(|s| match &s.value {
                                prometheus_parse::Value::Counter(v)
                                | prometheus_parse::Value::Gauge(v)
                                | prometheus_parse::Value::Untyped(v) => Some(*v as i64),
                                _ => None,
                            })
                            .unwrap_or(0)
                    } else {
                        0
                    };

                    let delta = curr_val.saturating_sub(prev_val);
                    if delta > 0 {
                        let mut deltas_map = HashMap::new();
                        deltas_map.insert(code, delta);
                        deltas.push(DeltaRow::new(endpoint, Some(relay_id), deltas_map));
                    }
                }
            }
            "cb_pbs_beacon_node_status_code_total" => {
                for sample in &curr.samples {
                    if sample.metric != "cb_pbs_beacon_node_status_code_total" {
                        continue;
                    }
                    let endpoint = match sample.labels.get("endpoint") {
                        Some(e) => e.to_string(),
                        None => continue,
                    };
                    let code = match sample.labels.get("http_status_code") {
                        Some(c) => c.to_string(),
                        None => continue,
                    };

                    let curr_val = match &sample.value {
                        prometheus_parse::Value::Counter(v)
                        | prometheus_parse::Value::Gauge(v)
                        | prometheus_parse::Value::Untyped(v) => *v as i64,
                        _ => continue,
                    };

                    let prev_val = if let Some(prev) = prev {
                        prev.samples
                            .iter()
                            .find(|s| {
                                s.metric == "cb_pbs_beacon_node_status_code_total"
                                    && s.labels.get("endpoint") == Some(&endpoint)
                                    && s.labels.get("http_status_code") == Some(&code)
                            })
                            .and_then(|s| match &s.value {
                                prometheus_parse::Value::Counter(v)
                                | prometheus_parse::Value::Gauge(v)
                                | prometheus_parse::Value::Untyped(v) => Some(*v as i64),
                                _ => None,
                            })
                            .unwrap_or(0)
                    } else {
                        0
                    };

                    let delta = curr_val.saturating_sub(prev_val);
                    if delta > 0 {
                        let mut deltas_map = HashMap::new();
                        deltas_map.insert(code, delta);
                        deltas.push(DeltaRow::new(endpoint, None, deltas_map));
                    }
                }
            }
            "cb_pbs_submit_block_v2_fallback_to_v1_total" => {
                for sample in &curr.samples {
                    if sample.metric != "cb_pbs_submit_block_v2_fallback_to_v1_total" {
                        continue;
                    }
                    let relay_id = sample
                        .labels
                        .get("relay_id")
                        .map(|r| r.to_string())
                        .unwrap_or_else(|| "unknown".to_string());

                    let curr_val = match &sample.value {
                        prometheus_parse::Value::Counter(v)
                        | prometheus_parse::Value::Gauge(v)
                        | prometheus_parse::Value::Untyped(v) => *v as i64,
                        _ => continue,
                    };

                    let prev_val = if let Some(prev) = prev {
                        prev.samples
                            .iter()
                            .find(|s| {
                                s.metric == "cb_pbs_submit_block_v2_fallback_to_v1_total"
                                    && s.labels.get("relay_id") == Some(&relay_id)
                            })
                            .and_then(|s| match &s.value {
                                prometheus_parse::Value::Counter(v)
                                | prometheus_parse::Value::Gauge(v)
                                | prometheus_parse::Value::Untyped(v) => Some(*v as i64),
                                _ => None,
                            })
                            .unwrap_or(0)
                    } else {
                        0
                    };

                    let delta = curr_val.saturating_sub(prev_val);
                    if delta > 0 {
                        let mut deltas_map = HashMap::new();
                        deltas_map.insert("fallback".to_string(), delta);
                        deltas.push(DeltaRow::new(
                            "submit_blinded_block".to_string(),
                            Some(relay_id),
                            deltas_map,
                        ));
                    }
                }
            }
            _ => continue,
        }
    }

    deltas
}

/// Format a Vec of DeltaRow into human-readable log lines.
pub fn format_delta_log(rows: &[DeltaRow]) -> Vec<String> {
    rows.iter().map(|r| r.to_log_line()).collect()
}

/// Format a Vec of DeltaRow into newline-delimited JSON.
pub fn format_delta_json(rows: &[DeltaRow]) -> Vec<String> {
    rows.iter().map(|r| r.to_json().to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Scrape {
        let lines = text.lines().map(|l| Ok(l.to_owned()));
        Scrape::parse(lines).expect("valid prometheus text")
    }

    #[test]
    fn compute_deltas_simple() {
        let prev = parse(
            r#"
# HELP cb_pbs_relay_status_code_total relay codes
# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{http_status_code="200",endpoint="get_header",relay_id="r0"} 10
"#,
        );

        let curr = parse(
            r#"
# HELP cb_pbs_relay_status_code_total relay codes
# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{http_status_code="200",endpoint="get_header",relay_id="r0"} 13
cb_pbs_relay_status_code_total{http_status_code="204",endpoint="get_header",relay_id="r0"} 5
"#,
        );

        let deltas = compute_deltas(Some(&prev), &curr, &["cb_pbs_relay_status_code_total"]);

        assert_eq!(deltas.len(), 2);
        let row1 = &deltas[0];
        assert_eq!(row1.endpoint, "get_header");
        assert_eq!(row1.relay, Some("r0".to_string()));
        assert_eq!(row1.deltas.get("200"), Some(&3));

        let row2 = &deltas[1];
        assert_eq!(row2.endpoint, "get_header");
        assert_eq!(row2.relay, Some("r0".to_string()));
        assert_eq!(row2.deltas.get("204"), Some(&5));
    }

    #[test]
    fn compute_deltas_reset() {
        let prev = parse(
            r#"
# HELP cb_pbs_relay_status_code_total relay codes
# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{http_status_code="200",endpoint="get_header",relay_id="r0"} 10
"#,
        );

        let curr = parse(
            r#"
# HELP cb_pbs_relay_status_code_total relay codes
# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{http_status_code="200",endpoint="get_header",relay_id="r0"} 8  // reset
"#,
        );

        let deltas = compute_deltas(Some(&prev), &curr, &["cb_pbs_relay_status_code_total"]);

        assert_eq!(deltas.len(), 0); // reset = zero delta
    }

    #[test]
    fn compute_deltas_missing_current() {
        let prev = parse(
            r#"
# HELP cb_pbs_relay_status_code_total relay codes
# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{http_status_code="200",endpoint="get_header",relay_id="r0"} 10
"#,
        );

        let curr = parse(
            r#"
# HELP cb_pbs_relay_status_code_total relay codes
# TYPE cb_pbs_relay_status_code_total counter
cb_pbs_relay_status_code_total{http_status_code="200",endpoint="get_header",relay_id="r1"} 15
"#,
        );

        let deltas = compute_deltas(Some(&prev), &curr, &["cb_pbs_relay_status_code_total"]);

        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].deltas.get("200"), Some(&15)); // r1: 15-0
    }

    #[test]
    fn format_delta_log_output() {
        let row = DeltaRow::new(
            "get_header".to_string(),
            Some("r0".to_string()),
            [("200".to_string(), 3), ("204".to_string(), 5)]
                .iter()
                .cloned()
                .collect(),
        );
        let lines = format_delta_log(&[row]);
        assert_eq!(lines[0], "live: get_header +3 200, +5 204 (r0)");
    }

    #[test]
    fn format_delta_json_output() {
        let row = DeltaRow::new(
            "get_header".to_string(),
            Some("r0".to_string()),
            [("200".to_string(), 3), ("204".to_string(), 5)]
                .iter()
                .cloned()
                .collect(),
        );
        let jsons = format_delta_json(&[row]);
        let json: serde_json::Value = serde_json::from_str(&jsons[0]).unwrap();
        assert_eq!(json["endpoint"], "get_header");
        assert_eq!(json["relay"], "r0");
        assert_eq!(json["deltas"]["200"], 3);
        assert_eq!(json["deltas"]["204"], 5);
    }
}
