//! Commit-Boost SIGNER module checks.
//!
//! The signer has never been runnable on a Kurtosis devnet (the ethereum-package
//! had no config support for it), so this is the first assertion of it here.
//!
//! **What we deliberately do NOT assert.** `GET /status` is
//! `Ok(StatusCode::OK)` with no logic - it returns 200 with zero keys loaded -
//! and the metrics server exposes a SECOND unconditional `/status`, so probing
//! the wrong port is an even emptier green. The startup log's
//! `loaded_consensus=N` is log-only (the signer registers exactly one metric,
//! `signer_status_code_total`, with no key-count gauge) and is ANSI-colored by
//! default, so the field is not even a contiguous substring.
//!
//! **What we assert instead:** a JWT-authenticated `GET /signer/v1/get_pubkeys`
//! with a COUNT assertion. One HTTP call subsumes liveness, module registration,
//! JWT auth AND key loading - and it is by construction the assertion that fails
//! if the keystore mount is unreadable, which is the failure this whole feature
//! is most likely to hit (CB's loader is `filter_map` + `warn!`, so a permissions
//! problem yields a healthy process holding zero keys).

use base64::Engine;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::checks::CheckResult;

/// CB's module-API route for listing consensus pubkeys. The JWT is bound to
/// this exact string; a mismatch is rejected.
pub const GET_PUBKEYS_ROUTE: &str = "/signer/v1/get_pubkeys";

/// CB's JWT lifetime (`SIGNER_JWT_EXPIRATION`); validation allows 10s leeway.
const JWT_EXPIRATION_SECS: u64 = 300;

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Mint the HS256 JWT a commit module presents to the signer (pure).
///
/// Claims are `{module, route, exp, payload_hash}`:
/// - `route` MUST equal the exact request path, else the signer rejects it.
/// - `payload_hash` MUST be `null` when the request has no body, and
///   `keccak256(body)` when it does. Both directions are enforced by CB, so a
///   "harmless" empty-string default would fail auth.
///
/// `now_secs` is a parameter rather than read from the clock so the output is
/// deterministic and testable.
pub fn mint_module_jwt(
    module_id: &str,
    secret: &str,
    route: &str,
    payload_hash: Option<&str>,
    now_secs: u64,
) -> String {
    let header = br#"{"alg":"HS256","typ":"JWT"}"#;
    let claims = serde_json::json!({
        "module": module_id,
        "route": route,
        "exp": now_secs + JWT_EXPIRATION_SECS,
        "payload_hash": payload_hash,
    });
    let signing_input = format!("{}.{}", b64(header), b64(claims.to_string().as_bytes()));

    let mut mac = <Hmac<Sha256>>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(signing_input.as_bytes());
    let sig = mac.finalize().into_bytes();

    format!("{signing_input}.{}", b64(&sig))
}

/// The shape of `GET /signer/v1/get_pubkeys`.
#[derive(Debug, Deserialize)]
pub struct PubkeysResponse {
    pub keys: Vec<PubkeyEntry>,
}

#[derive(Debug, Deserialize)]
pub struct PubkeyEntry {
    pub consensus: String,
}

/// Pure verdict for the signer key-loading assertion (Law 4 seam).
///
/// `expected` is how many validator keys the participant's keystore artifact
/// holds; `got` is what the signer actually reports.
///
/// A count of ZERO is the signature failure mode of this feature: CB's keystore
/// loaders skip unreadable or malformed entries with `filter_map` + `warn!`, so
/// a permissions problem (the devnet's `secrets/` dir is mode 600 and
/// root-owned) produces a perfectly healthy signer that has loaded nothing.
pub fn classify_signer_pubkeys(expected: usize, got: usize) -> CheckResult {
    let id = "signer.pubkeys";
    let data = serde_json::json!({ "expected_keys": expected, "loaded_keys": got });

    if got == 0 {
        return CheckResult::fail(
            id,
            1,
            format!(
                "the signer authenticated but loaded ZERO keys (expected {expected}). CB's keystore \
                 loaders skip unreadable entries silently, so suspect the mount: the devnet's \
                 `secrets/` dir is mode 600 root-owned and unreadable by the container's uid 10001 \
                 - use the teku-keys/teku-secrets pair"
            ),
        )
        .with_data(data);
    }
    if got != expected {
        return CheckResult::warn(
            id,
            1,
            format!(
                "signer loaded {got} key(s) but the keystore artifact holds {expected} - some \
                 keystores were skipped (CB warns and continues per key)"
            ),
        )
        .with_data(data);
    }
    CheckResult::pass(
        id,
        1,
        format!("signer loaded all {got} validator key(s) and authenticated the module JWT ✓"),
    )
    .with_data(data)
}

/// Pure verdict for the JWT negative control.
///
/// Run this LAST: CB rate-limits a source IP after `jwt_auth_fail_limit` (3)
/// failures for `jwt_auth_fail_timeout_seconds`, and every harness request
/// arrives from the same NAT address, so probing it early would 429 the positive
/// assertions that follow.
pub fn classify_jwt_rejection(status: u16) -> CheckResult {
    let id = "signer.jwt_auth";
    let data = serde_json::json!({ "status": status });
    match status {
        401 => CheckResult::pass(id, 2, "a bad module JWT is rejected with 401 ✓").with_data(data),
        429 => CheckResult::warn(
            id,
            2,
            "rate-limited (429) before the negative control could be observed - run negative \
             probes last, and lower CB_SIGNER_JWT_AUTH_FAIL_TIMEOUT_SECONDS",
        )
        .with_data(data),
        200 => CheckResult::fail(
            id,
            2,
            "a BAD module JWT was ACCEPTED (200) - signer authentication is not enforced",
        )
        .with_data(data),
        other => CheckResult::warn(
            id,
            2,
            format!("unexpected status {other} for a bad JWT (expected 401)"),
        )
        .with_data(data),
    }
}

/// Ask the signer for its pubkeys with a freshly minted module JWT.
pub async fn fetch_pubkeys(
    client: &reqwest::Client,
    signer_url: &str,
    module_id: &str,
    secret: &str,
    now_secs: u64,
) -> eyre::Result<(u16, Option<PubkeysResponse>)> {
    // No body on this route, so payload_hash MUST be null.
    let jwt = mint_module_jwt(module_id, secret, GET_PUBKEYS_ROUTE, None, now_secs);
    let resp = client
        .get(format!(
            "{}{GET_PUBKEYS_ROUTE}",
            signer_url.trim_end_matches('/')
        ))
        .bearer_auth(jwt)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Ok((status, None));
    }
    Ok((status, Some(resp.json().await?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_claims(jwt: &str) -> serde_json::Value {
        let payload = jwt.split('.').nth(1).expect("jwt has 3 parts");
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .expect("payload is base64url");
        serde_json::from_slice(&raw).expect("payload is json")
    }

    #[test]
    fn jwt_has_three_parts_and_is_deterministic() {
        let a = mint_module_jwt("TEST_MODULE", "secret", GET_PUBKEYS_ROUTE, None, 1000);
        let b = mint_module_jwt("TEST_MODULE", "secret", GET_PUBKEYS_ROUTE, None, 1000);
        assert_eq!(a, b, "same inputs must give the same token");
        assert_eq!(a.split('.').count(), 3);
    }

    #[test]
    fn jwt_binds_the_exact_route() {
        // CB compares `route` against the request path and rejects a mismatch,
        // so a token minted for one route cannot be replayed on another.
        let c = decode_claims(&mint_module_jwt("m", "s", GET_PUBKEYS_ROUTE, None, 0));
        assert_eq!(c["route"], GET_PUBKEYS_ROUTE);
        let other = decode_claims(&mint_module_jwt("m", "s", "/signer/v1/other", None, 0));
        assert_eq!(other["route"], "/signer/v1/other");
    }

    #[test]
    fn payload_hash_is_null_when_there_is_no_body() {
        // Enforced in BOTH directions by CB: a non-null hash on a bodyless
        // request is rejected just as a missing one on a request with a body is.
        let c = decode_claims(&mint_module_jwt("m", "s", GET_PUBKEYS_ROUTE, None, 0));
        assert!(
            c["payload_hash"].is_null(),
            "must be null, not \"\" or absent"
        );

        let c2 = decode_claims(&mint_module_jwt("m", "s", "/r", Some("0xabc"), 0));
        assert_eq!(c2["payload_hash"], "0xabc");
    }

    #[test]
    fn jwt_expiry_is_five_minutes_out() {
        let c = decode_claims(&mint_module_jwt("m", "s", "/r", None, 1_000_000));
        assert_eq!(c["exp"], 1_000_000 + 300);
    }

    #[test]
    fn jwt_signature_changes_with_the_secret() {
        let a = mint_module_jwt("m", "secret-a", "/r", None, 0);
        let b = mint_module_jwt("m", "secret-b", "/r", None, 0);
        assert_ne!(
            a, b,
            "a different module secret must produce a different token"
        );
        // Only the signature differs; the claims are identical.
        assert_eq!(a.rsplit_once('.').unwrap().0, b.rsplit_once('.').unwrap().0);
    }

    #[test]
    fn zero_keys_fails_and_names_the_permissions_trap() {
        // The signature failure of this feature: healthy process, no keys.
        let r = classify_signer_pubkeys(128, 0);
        assert_eq!(r.status, crate::checks::CheckStatus::Fail);
        assert!(r.detail.contains("ZERO keys"));
        assert!(r.detail.contains("teku"), "points at the fix: {}", r.detail);
    }

    #[test]
    fn partial_key_load_warns() {
        let r = classify_signer_pubkeys(128, 100);
        assert_eq!(r.status, crate::checks::CheckStatus::Warn);
        assert_eq!(r.data["loaded_keys"], 100);
    }

    #[test]
    fn full_key_load_passes() {
        let r = classify_signer_pubkeys(128, 128);
        assert_eq!(r.status, crate::checks::CheckStatus::Pass);
    }

    #[test]
    fn jwt_negative_control_verdicts() {
        assert_eq!(
            classify_jwt_rejection(401).status,
            crate::checks::CheckStatus::Pass
        );
        // Accepting a bad JWT is the security-relevant failure.
        assert_eq!(
            classify_jwt_rejection(200).status,
            crate::checks::CheckStatus::Fail
        );
        // 429 means we poisoned ourselves by probing too early.
        let limited = classify_jwt_rejection(429);
        assert_eq!(limited.status, crate::checks::CheckStatus::Warn);
        assert!(limited.detail.contains("run negative probes last"));
    }
}
