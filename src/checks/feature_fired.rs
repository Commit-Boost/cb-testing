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
    /// `skip_sigverify = true` (`[pbs]`). A negative codepath: signature
    /// verification is simply not called, with NO success log or metric. On the
    /// happy path (valid mock-relay signatures) ON is indistinguishable from OFF
    /// in the logs — so this cannot be positively confirmed without a
    /// bad-signature-injecting relay. Reported honestly, never falsely green.
    SkipSigverify,
}

/// Every feature we know how to check, in report order.
pub const ALL_FEATURES: [Feature; 3] = [
    Feature::TimingGames,
    Feature::ExtraValidation,
    Feature::SkipSigverify,
];

impl Feature {
    /// The check id emitted for this feature.
    pub fn id(self) -> &'static str {
        match self {
            Feature::TimingGames => "feature.timing_games",
            Feature::ExtraValidation => "feature.extra_validation",
            Feature::SkipSigverify => "feature.skip_sigverify",
        }
    }

    /// The boolean config key that enables the feature.
    pub fn config_key(self) -> &'static str {
        match self {
            Feature::TimingGames => "enable_timing_games",
            Feature::ExtraValidation => "extra_validation_enabled",
            Feature::SkipSigverify => "skip_sigverify",
        }
    }

    /// CB log-line markers that PROVE the codepath fired. Empty when no positive
    /// runtime proof exists (`skip_sigverify`).
    fn proof_markers(self) -> &'static [&'static str] {
        match self {
            Feature::TimingGames => &["TG:"],
            Feature::ExtraValidation => &["fetched parent block", "fetching parent block"],
            Feature::SkipSigverify => &[],
        }
    }

    /// Human name used in check details.
    fn label(self) -> &'static str {
        match self {
            Feature::TimingGames => "timing games",
            Feature::ExtraValidation => "extra validation",
            Feature::SkipSigverify => "skip sigverify",
        }
    }
}

/// Detect which features a CB config template enables (pure). Scans for a
/// `<key> = true` line, ignoring comments.
pub fn detect_enabled_features(template: &str) -> Vec<Feature> {
    ALL_FEATURES
        .into_iter()
        .filter(|f| config_enables(template, f.config_key()))
        .collect()
}

/// True iff `template` has an uncommented `key = true` line.
fn config_enables(template: &str, key: &str) -> bool {
    template.lines().any(|line| {
        let t = line.trim();
        if t.starts_with('#') {
            return false;
        }
        match t.split_once('=') {
            Some((k, v)) => k.trim() == key && v.trim().trim_matches('"') == "true",
            None => false,
        }
    })
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
    }
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

    #[test]
    fn feature_ids_and_keys_are_stable() {
        assert_eq!(Feature::TimingGames.id(), "feature.timing_games");
        assert_eq!(
            Feature::ExtraValidation.config_key(),
            "extra_validation_enabled"
        );
        assert_eq!(Feature::SkipSigverify.config_key(), "skip_sigverify");
    }
}
