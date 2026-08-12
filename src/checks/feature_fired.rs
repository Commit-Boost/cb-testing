//! Feature-fired assertions (Law 3): prove a toggled Commit-Boost feature's
//! codepath actually EXECUTED at runtime, not merely that the generic health
//! checks passed. Without these, the skip-sigverify / extra-validation /
//! timing-games scenarios are non-tests — they enable a feature and then only
//! assert the same things cb-basic does.
//!
//! The proof is CB debug logs (the scenarios all set `[logs.stdout] level =
//! "debug"`). Each feature that leaves a unique log marker gets a positive
//! assertion; `skip_sigverify` leaves NO positive trace on the happy path (it
//! is a *negative* codepath — a function simply not called, with no success log
//! or metric), so it is honestly reported as un-verifiable at runtime rather
//! than falsely green. See the per-variant docs on [`Feature`].
//!
//! Shape mirrors `mux_routing`: detect the feature from the CB config, fetch CB
//! logs, look for the marker; a missing marker WARNs (could be no getHeader in
//! the window) rather than FAILs — the same no-false-red discipline as the mux
//! check. The verdict logic is a pure seam (`classify_*`) for unit testing.

use crate::checks::CheckResult;
use crate::checks::mux_routing::fetch_filtered_logs;
use tracing::warn;

/// A CB feature whose activation we try to confirm fired at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    /// `enable_timing_games = true` (per-relay). Delays/repeats getHeader; emits
    /// DEBUG logs prefixed `TG:` on no other codepath.
    TimingGames,
    /// `extra_validation_enabled = true` (`[pbs]`). Fetches the parent block to
    /// validate the header; emits `"fetching parent block"` / `"fetched parent
    /// block"` DEBUG logs on no other codepath.
    ExtraValidation,
    /// `get_header = "stream"` (per-relay). getHeader rides a websocket bid
    /// stream instead of HTTP polling; CB logs
    /// `"received new header from ws stream"` on no other codepath. The HTTP
    /// FALLBACK (merged with the feature) means a broken stream still passes
    /// every MEV check silently - this marker check is the only discriminator.
    WsHeaderStream,
    /// `skip_sigverify = true` (`[pbs]`). A negative codepath: signature
    /// verification is simply not called, with NO success log or metric. On the
    /// happy path (valid mock-relay signatures) ON is indistinguishable from OFF
    /// in the logs — so this cannot be positively confirmed without a
    /// bad-signature-injecting relay. Reported honestly, never falsely green.
    SkipSigverify,
}

/// Every feature we know how to check, in report order.
pub const ALL_FEATURES: [Feature; 4] = [
    Feature::TimingGames,
    Feature::ExtraValidation,
    Feature::WsHeaderStream,
    Feature::SkipSigverify,
];

impl Feature {
    /// The check id emitted for this feature.
    pub fn id(self) -> &'static str {
        match self {
            Feature::TimingGames => "feature.timing_games",
            Feature::ExtraValidation => "feature.extra_validation",
            Feature::WsHeaderStream => "feature.ws_header_stream",
            Feature::SkipSigverify => "feature.skip_sigverify",
        }
    }

    /// The boolean config key that enables the feature.
    pub fn config_key(self) -> &'static str {
        match self {
            Feature::TimingGames => "enable_timing_games",
            Feature::ExtraValidation => "extra_validation_enabled",
            Feature::WsHeaderStream => "get_header",
            Feature::SkipSigverify => "skip_sigverify",
        }
    }

    /// The config VALUE that arms the feature. Most features are boolean
    /// toggles; `get_header` is a string transport selector.
    pub fn config_value(self) -> &'static str {
        match self {
            Feature::WsHeaderStream => "stream",
            _ => "true",
        }
    }

    /// CB log-line markers that PROVE the codepath fired. Empty when no positive
    /// runtime proof exists (`skip_sigverify`).
    fn proof_markers(self) -> &'static [&'static str] {
        match self {
            Feature::TimingGames => &["TG:"],
            Feature::ExtraValidation => &["fetched parent block", "fetching parent block"],
            Feature::WsHeaderStream => &[WS_STREAM_MARKER],
            Feature::SkipSigverify => &[],
        }
    }

    /// Human name used in check details.
    fn label(self) -> &'static str {
        match self {
            Feature::TimingGames => "timing games",
            Feature::ExtraValidation => "extra validation",
            Feature::WsHeaderStream => "ws header stream",
            Feature::SkipSigverify => "skip sigverify",
        }
    }
}

/// Detect which features a CB config template enables (pure). Scans for a
/// `<key> = true` line, ignoring comments.
pub fn detect_enabled_features(template: &str) -> Vec<Feature> {
    ALL_FEATURES
        .into_iter()
        .filter(|f| config_enables(template, f.config_key(), f.config_value()))
        .collect()
}

/// True iff `template` has an uncommented `key = <value>` line.
fn config_enables(template: &str, key: &str, value: &str) -> bool {
    template.lines().any(|line| {
        let t = line.trim();
        if t.starts_with('#') {
            return false;
        }
        match t.split_once('=') {
            Some((k, v)) => k.trim() == key && v.trim().trim_matches('"') == value,
            None => false,
        }
    })
}

/// CB's proof line for a bid received over the websocket stream — a stream
/// header can ONLY exist because the relay delivered it, so this single marker
/// proves the full relay->CB stream path (no separate relay-side check needed).
pub const WS_STREAM_MARKER: &str = "received new header from ws stream";
/// CB's warn line when a stream attempt degrades to HTTP.
pub const WS_FALLBACK_MARKER: &str = "falling back to http get_header";

/// Pure verdict for the stream-vs-fallback balance (Law 4 seam).
///
/// The HTTP fallback makes a broken stream invisible to every MEV check, so
/// the COUNT is the signal, not delivery. Thresholds are measured, not
/// guessed: a healthy 220-slot run (CB e622a5e + helix :main)
/// showed exactly ONE fallback — the first slot's registration-TOFU race
/// ("proposer not registered"), gone 12s later.
///
/// - 0 fallbacks                  -> PASS
/// - 1 fallback, stream served    -> PASS (the startup race; count reported)
/// - >1 fallback, stream served   -> WARN, degraded stream
/// - fallbacks, stream NEVER served -> WARN (annotative only: the marker check
///   already carries `inconclusive` for the armed-but-unproven feature, so the
///   red under --require-feature-proof comes from there, not double-flagged)
pub fn classify_ws_fallback(streamed: usize, fallbacks: usize) -> CheckResult {
    let id = "feature.ws_stream_fallback";
    let data = serde_json::json!({
        "streamed_headers": streamed,
        "fallbacks": fallbacks,
        "fallback_marker": WS_FALLBACK_MARKER,
    });
    match (streamed, fallbacks) {
        (_, 0) => CheckResult::pass(id, 2, "stream transport: zero HTTP fallbacks ✓"),
        (s, 1) if s > 0 => CheckResult::pass(
            id,
            2,
            format!(
                "stream served {s} header(s) with 1 HTTP fallback (the startup registration race; expected)"
            ),
        ),
        (s, f) if s > 0 => CheckResult::warn(
            id,
            2,
            format!(
                "stream DEGRADED: {f} HTTP fallbacks alongside {s} streamed header(s) - the                  stream is flapping; MEV checks stay green via the fallback, so this count is                  the only signal"
            ),
        ),
        (_, f) => CheckResult::warn(
            id,
            2,
            format!(
                "stream NEVER served: all getHeader traffic degraded to HTTP ({f} fallback                  warn(s)). The feature.ws_header_stream check carries the inconclusive flag                  for this run"
            ),
        ),
    }
    .with_data(data)
}

/// Pure verdict for a log-marker feature (timing-games / extra-validation).
/// `proof_count` = CB log lines matching the feature's proof markers.
///
/// - proof_count > 0 → PASS (the codepath demonstrably fired)
/// - proof_count == 0 → WARN (feature enabled in config but no proof marker
///   seen — the codepath may not have run, or no getHeader landed in the
///   window, or debug logging is off). NOT a FAIL: no-false-red.
pub fn classify_marker_feature(feature: Feature, proof_count: usize) -> CheckResult {
    let id = feature.id();
    let label = feature.label();
    let data = serde_json::json!({
        "feature": label,
        "config_key": feature.config_key(),
        "proof_markers": feature.proof_markers(),
        "proof_lines_seen": proof_count,
    });
    if proof_count > 0 {
        CheckResult::pass(
            id,
            1,
            format!(
                "{label} fired ✓ {proof_count} matching CB debug log line(s) prove the codepath ran"
            ),
        )
        .with_data(data)
    } else {
        CheckResult::warn(
            id,
            1,
            format!(
                "{label} is enabled in the CB config but ZERO proof markers ({:?}) were seen in \
                 CB debug logs — the codepath may not have fired (no getHeader in window?) or \
                 `[logs.stdout] level = \"debug\"` is off. NOT asserting the feature ran.",
                feature.proof_markers()
            ),
        )
        .with_data(data)
        .mark_inconclusive()
    }
}

/// The helix relay's signing pubkey (`DEFAULT_MEV_PUBKEY` in the
/// ethereum-package fork — a fixed constant of the devnet topology). A CB
/// `[[relays]]` url carrying any OTHER pubkey is the sigverify-differential
/// fault injection: CB's validate_signature would reject every bid from the
/// real relay, so a bid winning the auction proves the skip fired.
const HELIX_RELAY_PUBKEY: &str = "0xa55c1285d84ba83a5ad26420cd5ad3091e49c55a813eee651cd467db38a8c8e63192f47955e9376f6b42f6d190571cb5";

/// Detect the fault injection (pure): does any `[[relays]]` url in the CB
/// config template carry a pubkey that is NOT the helix relay's signing key?
pub fn has_poisoned_relay_pubkey(template: &str) -> bool {
    let helix = HELIX_RELAY_PUBKEY.to_lowercase();
    template.lines().any(|line| {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("url = ") else {
            return false;
        };
        let url = rest.trim_matches('"');
        // scheme://<pubkey>@host — extract the userinfo if present.
        let Some((_, after_scheme)) = url.split_once("://") else {
            return false;
        };
        match after_scheme.split_once('@') {
            Some((pubkey, _)) => pubkey.to_lowercase() != helix,
            None => false,
        }
    })
}

/// Pure verdict for `skip_sigverify` (Law 4 seam).
///
/// Without the fault injection (`poisoned = false`) the feature is a negative
/// codepath with no positive runtime signal — honest WARN, never a false green.
///
/// With a poisoned relay pubkey in the CB config, the differential becomes
/// real: CB's validate_signature would reject every bid from the real relay
/// (PubkeyMismatch), so `auction_winners > 0` is positive proof the skip
/// codepath fired — with sigverify on, zero bids could have won.
pub fn classify_skip_sigverify(poisoned: bool, auction_winners: usize) -> CheckResult {
    let id = Feature::SkipSigverify.id();
    if !poisoned {
        return CheckResult::warn(
            id,
            1,
            "skip_sigverify is enabled, but it is a negative codepath (signature verification is \
             simply not called) that emits no success log or metric. On the happy path (valid \
             relay signatures) it is indistinguishable from OFF, so it cannot be positively \
             confirmed at runtime. Run the cb-sigverify-diff scenario (wrong-pubkey relay url) \
             for a real differential. Not asserting either way.",
        )
        .with_data(serde_json::json!({
            "feature": "skip sigverify",
            "config_key": "skip_sigverify",
            "verifiable_at_runtime": false,
            "poisoned_relay": false,
        }));
    }

    let data = serde_json::json!({
        "feature": "skip sigverify",
        "config_key": "skip_sigverify",
        "verifiable_at_runtime": true,
        "poisoned_relay": true,
        "auction_winners": auction_winners,
    });
    if auction_winners > 0 {
        CheckResult::pass(
            id,
            1,
            format!(
                "skip_sigverify fired ✓ {auction_winners} auction winner(s) despite a \
                 wrong-pubkey relay url — with signature verification ON every bid would have \
                 been rejected (PubkeyMismatch), so bids winning proves the skip codepath ran"
            ),
        )
        .with_data(data)
    } else {
        CheckResult::warn(
            id,
            1,
            "skip_sigverify enabled with the wrong-pubkey relay url (differential armed) but \
             ZERO auction winners observed — cannot distinguish 'skip did not fire' from 'no \
             bids in the window'. NOT asserting the feature ran.",
        )
        .with_data(data)
        .mark_inconclusive()
    }
}

/// CB's rejection marker when a bid is under `min_bid_eth`
/// (`ValidationError::BidTooLow`, rendered by its `thiserror` Display and
/// surfaced by the `error!(%err, relay_id)` at the get_header call site).
const BID_TOO_LOW_MARKER: &str = "bid below minimum";

/// Parse `min_bid_eth = <x>` out of the CB config template (pure). Returns the
/// floor in ETH, or `None` when the key is absent or zero (zero = no floor).
pub fn detect_min_bid_eth(template: &str) -> Option<f64> {
    template.lines().find_map(|line| {
        let t = line.trim();
        if t.starts_with('#') {
            return None;
        }
        let (k, v) = t.split_once('=')?;
        if k.trim() != "min_bid_eth" {
            return None;
        }
        let val: f64 = v.trim().trim_matches('"').parse().ok()?;
        (val > 0.0).then_some(val)
    })
}

/// Pure verdict for the `min_bid_eth` floor (Law 4 seam).
///
/// `rejections` = CB log lines carrying [`BID_TOO_LOW_MARKER`].
/// `winner_values_eth` = the `value_eth` of every `auction winner` line.
///
/// The definitive falsifier is a WINNER BELOW THE FLOOR: that can only happen
/// if the floor was not applied, which is the failure mode that matters here.
/// `[pbs]` has no `deny_unknown_fields` (it must `#[serde(flatten)]`), so a
/// renamed or misspelled key is SILENTLY IGNORED rather than rejected - this
/// check is the canary for that whole class.
///
/// Absence of rejections is NOT a failure on its own: every bid legitimately
/// clearing the floor looks the same as a dropped key, so that is a WARN naming
/// both possibilities rather than a false red.
pub fn classify_min_bid(
    floor_eth: f64,
    rejections: usize,
    winner_values_eth: &[f64],
) -> CheckResult {
    let id = "feature.min_bid";
    let below: Vec<f64> = winner_values_eth
        .iter()
        .copied()
        .filter(|v| *v < floor_eth)
        .collect();
    let data = serde_json::json!({
        "feature": "min bid",
        "config_key": "min_bid_eth",
        "floor_eth": floor_eth,
        "rejections": rejections,
        "auction_winners": winner_values_eth.len(),
        "winners_below_floor": below.len(),
    });

    if !below.is_empty() {
        return CheckResult::fail(
            id,
            1,
            format!(
                "{} auction winner(s) had a value BELOW the {floor_eth} ETH floor (lowest {:.6})                  -- min_bid_eth was not applied. `[pbs]` silently ignores unknown keys, so suspect                  a renamed/misspelled field before suspecting CB",
                below.len(),
                below.iter().cloned().fold(f64::INFINITY, f64::min)
            ),
        )
        .with_data(data);
    }
    if rejections > 0 {
        return CheckResult::pass(
            id,
            1,
            format!(
                "min_bid_eth enforced ✓ {rejections} bid(s) rejected below the {floor_eth} ETH                  floor, and no winner was under it"
            ),
        )
        .with_data(data);
    }
    CheckResult::warn(
        id,
        1,
        format!(
            "min_bid_eth = {floor_eth} is set but ZERO bids were rejected -- cannot distinguish              'the floor was silently ignored' from 'every bid legitimately cleared it' (is the              builder subsidy raising bids above the floor?). NOT asserting the floor was applied"
        ),
    )
    .with_data(data)
        .mark_inconclusive()
}

/// Run the feature-fired checks for every feature the CB config enables.
///
/// `template` is the CB config TOML (via `mux_routing::read_cb_config_template`).
/// Emits one CheckResult per enabled feature; features not enabled are silent.
pub async fn run_feature_checks(
    enclave: &str,
    cb_service_names: &[String],
    template: &str,
) -> Vec<CheckResult> {
    let mut out = Vec::new();
    for feature in detect_enabled_features(template) {
        out.push(check_one(enclave, cb_service_names, feature, template).await);
    }
    // The stream/fallback balance is a COUNT comparison, not a marker check,
    // so it sits beside the per-feature loop.
    if detect_enabled_features(template).contains(&Feature::WsHeaderStream) {
        let streamed = count_log_lines(enclave, cb_service_names, &[WS_STREAM_MARKER]);
        let fallbacks = count_log_lines(enclave, cb_service_names, &[WS_FALLBACK_MARKER]);
        out.push(classify_ws_fallback(streamed, fallbacks));
    }
    // min_bid_eth is a VALUE knob, not a boolean toggle, so it sits outside the
    // Feature enum; it is only emitted when a floor is actually configured.
    if let Some(floor) = detect_min_bid_eth(template) {
        let rejections = count_log_lines(enclave, cb_service_names, &[BID_TOO_LOW_MARKER]);
        let winners = auction_winner_values_eth(enclave, cb_service_names);
        out.push(classify_min_bid(floor, rejections, &winners));
    }
    out
}

/// Collect the `value_eth` of every `auction winner` CB logged, as f64 ETH.
/// Lines whose value will not parse are skipped rather than failing the check.
fn auction_winner_values_eth(enclave: &str, cb_service_names: &[String]) -> Vec<f64> {
    let mut out = Vec::new();
    for service in cb_service_names {
        let Ok(logs) = fetch_filtered_logs(enclave, service, &["auction winner"]) else {
            continue;
        };
        for line in logs.lines() {
            if let Some(ev) = crate::checks::mux_routing::parse_cb_log_line(line)
                && ev.message.starts_with("auction winner")
                && let Some(v) = ev
                    .fields
                    .get("value_eth")
                    .and_then(|v| v.parse::<f64>().ok())
            {
                out.push(v);
            }
        }
    }
    out
}

async fn check_one(
    enclave: &str,
    cb_service_names: &[String],
    feature: Feature,
    template: &str,
) -> CheckResult {
    if feature == Feature::SkipSigverify {
        let poisoned = has_poisoned_relay_pubkey(template);
        // An auction winner is a bid that SURVIVED validation (CB only logs
        // "auction winner" for responses that made it out of validation), so
        // with a poisoned relay pubkey it is the positive skip-fired signal.
        let winners = if poisoned {
            count_log_lines(enclave, cb_service_names, &["auction winner"])
        } else {
            0
        };
        return classify_skip_sigverify(poisoned, winners);
    }

    let proof_count = count_log_lines(enclave, cb_service_names, feature.proof_markers());
    classify_marker_feature(feature, proof_count)
}

/// Count non-empty CB log lines matching any of `keywords` across services.
fn count_log_lines(enclave: &str, cb_service_names: &[String], keywords: &[&str]) -> usize {
    let mut count = 0usize;
    for service in cb_service_names {
        match fetch_filtered_logs(enclave, service, keywords) {
            Ok(logs) => {
                count += logs.lines().filter(|l| !l.trim().is_empty()).count();
            }
            Err(e) => warn!("feature check: failed to fetch logs from '{service}': {e}"),
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckStatus;

    #[test]
    fn detects_each_feature_from_its_key() {
        assert_eq!(
            detect_enabled_features("[pbs]\nskip_sigverify = true\n"),
            vec![Feature::SkipSigverify]
        );
        assert_eq!(
            detect_enabled_features("extra_validation_enabled = true\nrpc_url = \"x\"\n"),
            vec![Feature::ExtraValidation]
        );
        assert_eq!(
            detect_enabled_features("enable_timing_games = true\n"),
            vec![Feature::TimingGames]
        );
    }

    #[test]
    fn detects_multiple_features_in_report_order() {
        let template = "enable_timing_games = true\nskip_sigverify = true\n";
        // ALL_FEATURES order is timing, extra, skip — timing before skip.
        assert_eq!(
            detect_enabled_features(template),
            vec![Feature::TimingGames, Feature::SkipSigverify]
        );
    }

    #[test]
    fn baseline_config_enables_nothing() {
        let template = "[pbs]\nport = 18550\ntimeout_get_header_ms = 950\n";
        assert!(detect_enabled_features(template).is_empty());
    }

    #[test]
    fn false_and_commented_keys_do_not_count() {
        assert!(detect_enabled_features("skip_sigverify = false\n").is_empty());
        assert!(detect_enabled_features("# skip_sigverify = true\n").is_empty());
        // A key whose name is a superstring must not match.
        assert!(detect_enabled_features("not_skip_sigverify = true\n").is_empty());
    }

    #[test]
    fn quoted_true_value_still_counts() {
        // Tolerate `key = "true"` as well as `key = true`.
        assert_eq!(
            detect_enabled_features("skip_sigverify = \"true\"\n"),
            vec![Feature::SkipSigverify]
        );
    }

    #[test]
    fn marker_feature_passes_when_proof_seen() {
        let r = classify_marker_feature(Feature::TimingGames, 3);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.id, "feature.timing_games");
        assert_eq!(r.data["proof_lines_seen"], 3);
    }

    #[test]
    fn marker_feature_warns_when_no_proof() {
        // The Law-3 anti-false-green: enabled but unobserved is WARN, not PASS.
        let r = classify_marker_feature(Feature::ExtraValidation, 0);
        assert_eq!(r.status, CheckStatus::Warn);
        assert_eq!(r.id, "feature.extra_validation");
        assert!(r.detail.contains("NOT asserting"));
    }

    #[test]
    fn skip_sigverify_unpoisoned_is_an_honest_warn() {
        // Without the fault injection there is still no positive signal.
        let r = classify_skip_sigverify(false, 0);
        assert_eq!(r.status, CheckStatus::Warn);
        assert_eq!(r.id, "feature.skip_sigverify");
        assert_eq!(r.data["verifiable_at_runtime"], false);
        // Even a nonzero winner count proves nothing when unpoisoned (valid
        // signatures win auctions with sigverify ON too).
        assert_eq!(classify_skip_sigverify(false, 33).status, CheckStatus::Warn);
    }

    #[test]
    fn skip_sigverify_poisoned_with_winners_is_positive_proof() {
        // The differential: wrong-pubkey relay + bids winning = skip fired.
        let r = classify_skip_sigverify(true, 12);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.data["auction_winners"], 12);
        assert!(r.detail.contains("fired"), "detail: {}", r.detail);
    }

    #[test]
    fn skip_sigverify_poisoned_without_winners_warns() {
        // Armed but unobserved: could be no traffic, not a false green.
        let r = classify_skip_sigverify(true, 0);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.contains("NOT asserting"), "detail: {}", r.detail);
    }

    #[test]
    fn poisoned_relay_detection_both_sides() {
        // The real helix pubkey → not poisoned.
        let clean =
            format!("[[relays]]\nurl = \"http://{HELIX_RELAY_PUBKEY}@helix-relay-2:4040\"\n");
        assert!(!has_poisoned_relay_pubkey(&clean));
        // Any other pubkey → poisoned (this is the cb-sigverify-diff shape).
        let poisoned = "[[relays]]\nurl = \"http://0xaaf6c1251e73fb600624937760fef218aace5b253bf068ed45398aeb29d821e4d2899343ddcbbe37cb3f6cf500dff26c@helix-relay-2:4040\"\n";
        assert!(has_poisoned_relay_pubkey(poisoned));
        // Case-insensitive on the pubkey hex.
        let upper = format!(
            "url = \"http://{}@helix-relay-2:4040\"",
            HELIX_RELAY_PUBKEY.to_uppercase().replace("0X", "0x")
        );
        assert!(!has_poisoned_relay_pubkey(&upper));
        // A templated url ({{ $relay }}) has no userinfo → not poisoned.
        assert!(!has_poisoned_relay_pubkey("url = \"{{ $relay }}\""));
        // No relays at all → not poisoned.
        assert!(!has_poisoned_relay_pubkey("[pbs]\nport = 18550\n"));
    }

    // --- min_bid: the floor knob + the silent-flatten canary ----------------

    #[test]
    fn detects_min_bid_floor_and_ignores_zero_or_absent() {
        assert_eq!(detect_min_bid_eth("[pbs]\nmin_bid_eth = 0.5\n"), Some(0.5));
        assert_eq!(detect_min_bid_eth("min_bid_eth = \"0.25\"\n"), Some(0.25));
        // zero means "no floor" - must not emit a check at all
        assert_eq!(detect_min_bid_eth("min_bid_eth = 0\n"), None);
        assert_eq!(detect_min_bid_eth("[pbs]\nport = 18550\n"), None);
        assert_eq!(detect_min_bid_eth("# min_bid_eth = 0.5\n"), None);
    }

    #[test]
    fn min_bid_passes_when_bids_were_rejected_and_no_winner_is_under_the_floor() {
        let r = classify_min_bid(0.5, 30, &[]);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.data["rejections"], 30);
    }

    #[test]
    fn min_bid_fails_when_a_winner_is_below_the_floor() {
        // The definitive falsifier: a sub-floor bid winning can ONLY happen if
        // the floor was not applied - the silent-flatten trap firing.
        let r = classify_min_bid(0.5, 0, &[0.04, 0.9]);
        assert_eq!(r.status, CheckStatus::Fail);
        assert_eq!(r.data["winners_below_floor"], 1);
        assert!(
            r.detail.contains("silently ignores"),
            "names the trap: {}",
            r.detail
        );
    }

    #[test]
    fn min_bid_fail_beats_rejections() {
        // Even with rejections present, a single sub-floor winner is fatal.
        let r = classify_min_bid(0.5, 10, &[0.01]);
        assert_eq!(r.status, CheckStatus::Fail);
    }

    #[test]
    fn min_bid_warns_when_nothing_was_rejected() {
        // Cannot distinguish "key ignored" from "every bid cleared the floor"
        // (e.g. the builder subsidy lifting bids) - no false red.
        let r = classify_min_bid(0.5, 0, &[1.04, 2.04]);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.detail.contains("NOT asserting"));
        assert_eq!(r.data["winners_below_floor"], 0);
    }

    #[test]
    fn feature_ids_and_keys_are_stable() {
        assert_eq!(Feature::TimingGames.id(), "feature.timing_games");
        assert_eq!(
            Feature::ExtraValidation.config_key(),
            "extra_validation_enabled"
        );
        assert_eq!(Feature::SkipSigverify.config_key(), "skip_sigverify");
    }

    // --- inconclusive marking (Law 3) -----------------------------------
    //
    // These verdicts are tier-1 WARN, which `exit_code` treats as pass. The
    // `inconclusive` flag is what lets `--require-feature-proof` tell "the
    // experiment produced no signal" apart from "a benign anomaly was noted",
    // so which sites carry it IS the contract.

    // Contract: feature enabled but ZERO proof markers = armed and unmeasured.
    // Contract: `get_header = "stream"` (a string knob, unlike the boolean
    // features) arms WsHeaderStream; plain http does not.
    #[test]
    fn stream_transport_arms_ws_feature() {
        let t = "[pbs]\nport = 1\n[[relays]]\nget_header = \"stream\"\n";
        assert!(detect_enabled_features(t).contains(&Feature::WsHeaderStream));
        let t2 = "[[relays]]\nget_header = \"http\"\n";
        assert!(!detect_enabled_features(t2).contains(&Feature::WsHeaderStream));
        // commented-out lines never arm
        let t3 = "# get_header = \"stream\"\n";
        assert!(!detect_enabled_features(t3).contains(&Feature::WsHeaderStream));
    }

    // Contract: the fallback verdict thresholds, measured on a 220-slot
    // healthy run = exactly one startup-race fallback).
    #[test]
    fn ws_fallback_thresholds() {
        use crate::checks::CheckStatus;
        // zero fallbacks: clean pass
        assert_eq!(classify_ws_fallback(220, 0).status, CheckStatus::Pass);
        // the startup race: still a pass, count reported
        let r = classify_ws_fallback(220, 1);
        assert_eq!(r.status, CheckStatus::Pass);
        assert_eq!(r.data["fallbacks"], 1);
        // flapping stream: warn, NOT inconclusive (real observation)
        let r = classify_ws_fallback(200, 7);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(!r.inconclusive);
        // stream never served: warn, and NOT inconclusive here - the marker
        // check owns the inconclusive flag, this must not double-flag
        let r = classify_ws_fallback(0, 64);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            !r.inconclusive,
            "double-flagging would make one failure two"
        );
    }

    // Contract: the marker strings match CB main's actual log lines (pinned
    // from a live run; if CB rewords them, this is the place that
    // must fail).
    #[test]
    fn ws_marker_strings_pinned() {
        assert_eq!(WS_STREAM_MARKER, "received new header from ws stream");
        assert_eq!(WS_FALLBACK_MARKER, "falling back to http get_header");
    }

    #[test]
    fn zero_proof_markers_is_inconclusive() {
        let r = classify_marker_feature(Feature::ExtraValidation, 0);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            r.inconclusive,
            "zero proof markers proves nothing: {}",
            r.detail
        );
    }

    // Contract: proof markers seen = a real positive assertion.
    #[test]
    fn seen_proof_markers_is_conclusive() {
        let r = classify_marker_feature(Feature::ExtraValidation, 3);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(!r.inconclusive);
    }

    // Contract: the differential was ARMED (poisoned relay) and saw no winners,
    // so it measured nothing.
    #[test]
    fn armed_sigverify_differential_with_no_winners_is_inconclusive() {
        let r = classify_skip_sigverify(true, 0);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.inconclusive);
    }

    // Contract: the UNPOISONED case is structurally unconfirmable, not a failure
    // to measure. It must NOT be marked, or every plain scenario carrying
    // skip_sigverify would turn red under --require-feature-proof.
    #[test]
    fn unpoisoned_sigverify_is_warn_but_not_inconclusive() {
        let r = classify_skip_sigverify(false, 0);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(
            !r.inconclusive,
            "a negative codepath that cannot be observed is an honest WARN, not an unmeasured one"
        );
    }

    // Contract: a floor with zero rejections cannot separate "silently ignored"
    // from "every bid legitimately cleared it".
    #[test]
    fn min_bid_with_zero_rejections_is_inconclusive() {
        let r = classify_min_bid(0.5, 0, &[1.0, 2.0]);
        assert_eq!(r.status, CheckStatus::Warn);
        assert!(r.inconclusive);
    }

    // Contract: rejections observed = the floor demonstrably applied.
    #[test]
    fn min_bid_with_rejections_is_conclusive() {
        let r = classify_min_bid(0.5, 4, &[1.0]);
        assert_eq!(r.status, CheckStatus::Pass);
        assert!(!r.inconclusive);
    }

    // Contract: a winner UNDER the floor is a hard FAIL and never inconclusive.
    // That is evidence, not the absence of it.
    #[test]
    fn min_bid_violation_is_a_fail_not_inconclusive() {
        let r = classify_min_bid(0.5, 0, &[0.1]);
        assert_eq!(r.status, CheckStatus::Fail);
        assert!(!r.inconclusive);
    }
}
